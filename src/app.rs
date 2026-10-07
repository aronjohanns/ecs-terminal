use crate::aws::{self, Ecs, Profile};
use crate::tree::{Kind, Node, Row, Tree};
use anyhow::Result;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// Results coming back from background `aws` calls.
pub enum Msg {
    Children(Vec<usize>, Result<Vec<Node>>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Focus {
    Tree,
    Commands,
}

#[derive(Debug, Clone)]
pub struct CliCommand {
    pub title: String,
    pub command: String,
}

pub enum Screen {
    Profiles,
    Explorer,
}

pub struct App {
    pub screen: Screen,
    pub profiles: Vec<Profile>,
    pub profile_selected: usize,

    pub ecs: Option<Ecs>,
    pub tree: Tree,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub focus: Focus,

    pub commands: Vec<CliCommand>,
    pub command_selected: usize,

    pub region_input: Option<String>,
    /// Editable command text shown in a modal before running.
    pub command_input: Option<String>,
    /// Command to execute in the real terminal; consumed by the main loop.
    pub pending_run: Option<String>,
    pub status: String,
    pub should_quit: bool,
    /// Printed to stdout after the terminal is restored.
    pub output_on_exit: Option<String>,

    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl App {
    pub fn new(profile: Option<String>, region: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut profiles = aws::load_profiles();
        if let Some(p) = &profile {
            if !profiles.iter().any(|x| &x.name == p) {
                profiles.insert(0, Profile { name: p.clone(), region: None });
            }
        }
        let mut app = App {
            screen: Screen::Profiles,
            profiles,
            profile_selected: 0,
            ecs: None,
            tree: Tree::new(String::new()),
            rows: vec![],
            selected: 0,
            focus: Focus::Tree,
            commands: vec![],
            command_selected: 0,
            region_input: None,
            command_input: None,
            pending_run: None,
            status: String::new(),
            should_quit: false,
            output_on_exit: None,
            tx,
            rx,
        };
        if let Some(p) = profile {
            let idx = app.profiles.iter().position(|x| x.name == p).unwrap_or(0);
            app.profile_selected = idx;
            app.open_profile(region);
        }
        app
    }

    fn fallback_region() -> String {
        std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".into())
    }

    pub fn open_profile(&mut self, region_override: Option<String>) {
        let Some(p) = self.profiles.get(self.profile_selected).cloned() else { return };
        let region = region_override.or(p.region).unwrap_or_else(Self::fallback_region);
        self.ecs = Some(Ecs { profile: p.name, region: region.clone() });
        self.tree = Tree::new(region);
        self.selected = 1; // the ECS row
        self.commands.clear();
        self.screen = Screen::Explorer;
        self.status = "↑↓/jk move  Tab expand/collapse  ←→/hl switch panel  Enter run  y copy  e edit  r region  p profile  q quit".into();
        self.load_children(vec![0]);
        self.refresh_rows();
    }

    pub fn set_region(&mut self, region: String) {
        if let Some(ecs) = &mut self.ecs {
            ecs.region = region.clone();
        }
        self.tree = Tree::new(region);
        self.selected = 1;
        self.commands.clear();
        self.load_children(vec![0]);
        self.refresh_rows();
    }

    pub fn refresh_rows(&mut self) {
        self.rows = self.tree.rows();
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
        self.rebuild_commands();
    }

    pub fn selected_node(&self) -> Option<(&Row, &Node)> {
        let row = self.rows.get(self.selected)?;
        if row.message.is_some() {
            return None;
        }
        Some((row, self.tree.get(&row.path)?))
    }

    // ----- background loading -------------------------------------------------

    fn load_children(&mut self, path: Vec<usize>) {
        let Some(ecs) = self.ecs.clone() else { return };
        let Some(node) = self.tree.get_mut(&path) else { return };
        if node.loading {
            return;
        }
        node.loading = true;
        node.error = None;
        let kind = node.kind.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = load(&ecs, &kind);
            let _ = tx.send(Msg::Children(path, res));
        });
    }

    /// Drain background results. Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            changed = true;
            match msg {
                Msg::Children(path, res) => {
                    if let Some(node) = self.tree.get_mut(&path) {
                        node.loading = false;
                        match res {
                            Ok(children) => node.children = Some(children),
                            Err(e) => node.error = Some(format!("{e:#}")),
                        }
                    }
                    self.refresh_rows();
                }
            }
        }
        changed
    }

    // ----- navigation -----------------------------------------------------------

    pub fn move_selection(&mut self, delta: i32) {
        match self.focus {
            Focus::Tree => {
                let len = self.rows.len() as i32;
                if len == 0 {
                    return;
                }
                self.selected = (self.selected as i32 + delta).clamp(0, len - 1) as usize;
                self.rebuild_commands();
            }
            Focus::Commands => {
                let len = self.commands.len() as i32;
                if len == 0 {
                    return;
                }
                self.command_selected = (self.command_selected as i32 + delta).clamp(0, len - 1) as usize;
            }
        }
    }

    pub fn expand_or_activate(&mut self) {
        let Some((row, node)) = self.selected_node() else { return };
        let path = row.path.clone();
        if node.is_leaf() || node.expanded {
            // Enter on an open node focuses its commands.
            if !self.commands.is_empty() {
                self.focus = Focus::Commands;
            }
            return;
        }
        let needs_load = node.children.is_none() || node.error.is_some();
        if let Some(n) = self.tree.get_mut(&path) {
            n.expanded = true;
        }
        if needs_load {
            self.load_children(path);
        }
        self.refresh_rows();
    }

    /// Tab: open a closed node, close an open one.
    pub fn toggle_expand(&mut self) {
        let Some((_, node)) = self.selected_node() else { return };
        if node.expanded && !node.is_leaf() {
            self.collapse();
        } else {
            self.expand_or_activate();
        }
    }

    pub fn collapse(&mut self) {
        let Some(row) = self.rows.get(self.selected) else { return };
        let path = row.path.clone();
        let is_msg = row.message.is_some();
        let expanded = self.tree.get(&path).map(|n| n.expanded && !n.is_leaf()).unwrap_or(false);
        if expanded && !is_msg {
            if let Some(n) = self.tree.get_mut(&path) {
                n.expanded = false;
            }
        } else if let Some(parent) = path.split_last().map(|(_, p)| p.to_vec()) {
            // Jump to parent row.
            if let Some(i) = self.rows.iter().position(|r| r.message.is_none() && r.path == parent) {
                self.selected = i;
            }
        }
        self.refresh_rows();
    }

    pub fn reload_selected(&mut self) {
        let Some((row, node)) = self.selected_node() else { return };
        if node.is_leaf() {
            return;
        }
        let path = row.path.clone();
        if let Some(n) = self.tree.get_mut(&path) {
            n.expanded = true;
            n.children = None;
        }
        self.load_children(path);
        self.refresh_rows();
    }

    /// Enter on a command: fill in missing pieces first, then run.
    pub fn activate_command(&mut self) {
        let Some(c) = self.selected_command().cloned() else { return };
        if c.command.contains('<') && c.command.contains('>') {
            self.command_input = Some(c.command);
            return;
        }
        self.pending_run = Some(c.command);
    }

    pub fn edit_command(&mut self) {
        if let Some(c) = self.selected_command().cloned() {
            self.command_input = Some(c.command);
        }
    }

    // ----- commands -------------------------------------------------------------

    pub fn rebuild_commands(&mut self) {
        self.commands.clear();
        self.command_selected = 0;
        let Some(ecs) = self.ecs.clone() else { return };
        let pre = ecs.prefix();
        let Some((_, node)) = self.selected_node() else { return };
        let mut cmds = Vec::new();
        match &node.kind {
            Kind::Region { .. } | Kind::Ecs => {
                cmds.push(CliCommand { title: "List clusters".into(), command: format!("aws ecs list-clusters {pre}") });
            }
            Kind::Cluster(c) => {
                cmds.push(CliCommand {
                    title: "List services".into(),
                    command: format!("aws ecs list-services --cluster {} {pre}", c.name),
                });
                cmds.push(CliCommand {
                    title: "Describe cluster".into(),
                    command: format!("aws ecs describe-clusters --clusters {} {pre}", c.name),
                });
            }
            Kind::Service(s) => {
                let cluster = cluster_name(&s.cluster_arn);
                cmds.push(CliCommand {
                    title: "List running tasks".into(),
                    command: format!(
                        "aws ecs list-tasks --cluster {cluster} --service-name {} --desired-status RUNNING {pre}",
                        s.name
                    ),
                });
                cmds.push(CliCommand {
                    title: "Describe service".into(),
                    command: format!("aws ecs describe-services --cluster {cluster} --services {} {pre}", s.name),
                });
                cmds.push(CliCommand {
                    title: "Describe task definition".into(),
                    command: format!("aws ecs describe-task-definition --task-definition {} {pre}", s.task_definition),
                });
                cmds.push(toggle_exec(&cluster, s, &pre));
            }
            Kind::Container { name, cluster, service, .. } => {
                cmds.push(CliCommand {
                    title: "List running tasks".into(),
                    command: format!(
                        "aws ecs list-tasks --cluster {} --service-name {} --desired-status RUNNING {pre}",
                        cluster.name, service.name
                    ),
                });
                cmds.push(CliCommand {
                    title: "Open shell in container (pick a task below for a ready-made command)".into(),
                    command: format!(
                        "aws ecs execute-command --cluster {} --task <TASK_ID> --container {name} --interactive --command \"/bin/sh\" {pre}",
                        cluster.name
                    ),
                });
                if !service.enable_execute_command {
                    cmds.push(toggle_exec(&cluster.name, service, &pre));
                }
            }
            Kind::Task { task, container, cluster, service, task_role_arn } => {
                let exec = format!(
                    "aws ecs execute-command --cluster {} --task {} --container {container} --interactive",
                    cluster.name,
                    task.id()
                );
                cmds.push(CliCommand { title: "Open shell in container".into(), command: format!("{exec} --command \"/bin/sh\" {pre}") });
                cmds.push(CliCommand {
                    title: "Run a command in container".into(),
                    command: format!("{exec} --command \"<COMMAND>\" {pre}"),
                });
                cmds.push(CliCommand {
                    title: "Describe task".into(),
                    command: format!("aws ecs describe-tasks --cluster {} --tasks {} {pre}", cluster.name, task.id()),
                });
                cmds.push(CliCommand {
                    title: "Stop task".into(),
                    command: format!("aws ecs stop-task --cluster {} --task {} {pre}", cluster.name, task.id()),
                });
                if !service.enable_execute_command {
                    cmds.push(toggle_exec(&cluster.name, service, &pre));
                }
                if let Some(role) = task_role_arn {
                    cmds.push(CliCommand {
                        title: "Check task role has SSM permissions (exec prerequisite)".into(),
                        command: format!(
                            "aws iam simulate-principal-policy --policy-source-arn {role} --action-names ssmmessages:CreateControlChannel ssmmessages:CreateDataChannel ssmmessages:OpenControlChannel ssmmessages:OpenDataChannel --profile {} --region {}",
                            ecs.profile, ecs.region
                        ),
                    });
                }
            }
        }
        self.commands = cmds;
    }

    pub fn selected_command(&self) -> Option<&CliCommand> {
        self.commands.get(self.command_selected)
    }

    pub fn copy_selected_command(&mut self) {
        let Some(c) = self.selected_command().cloned() else { return };
        match copy_to_clipboard(&c.command) {
            Ok(()) => self.status = format!("Copied: {}", c.title),
            Err(e) => self.status = format!("Copy failed: {e}"),
        }
    }
}

fn cluster_name(arn: &str) -> String {
    arn.rsplit('/').next().unwrap_or(arn).to_string()
}

/// Mirrors `Service.toggleExecuteCommand` in the reference.
fn toggle_exec(cluster: &str, s: &aws::Service, pre: &str) -> CliCommand {
    let (title, flag) = if s.enable_execute_command {
        ("Disable command execution (redeploys service)", "--no-enable-execute-command")
    } else {
        ("Enable command execution (redeploys service)", "--enable-execute-command")
    };
    CliCommand {
        title: title.into(),
        command: format!(
            "aws ecs update-service --cluster {cluster} --service {} {flag} --force-new-deployment {pre}",
            s.name
        ),
    }
}

fn load(ecs: &Ecs, kind: &Kind) -> Result<Vec<Node>> {
    Ok(match kind {
        Kind::Ecs => ecs.list_clusters()?.into_iter().map(|c| Node::new(Kind::Cluster(c))).collect(),
        Kind::Cluster(c) => ecs.list_services(&c.arn)?.into_iter().map(|s| Node::new(Kind::Service(s))).collect(),
        Kind::Service(s) => {
            // Two independent CLI round-trips; run them concurrently.
            let (td, tasks) = thread::scope(|sc| {
                let td = sc.spawn(|| ecs.describe_task_definition(&s.task_definition));
                let tasks = sc.spawn(|| ecs.list_tasks(&s.cluster_arn, &s.name));
                (td.join().expect("thread panicked"), tasks.join().expect("thread panicked"))
            });
            let (td, tasks) = (td?, tasks?);
            let cluster = aws::Cluster { arn: s.cluster_arn.clone(), name: cluster_name(&s.cluster_arn) };
            td.containers
                .into_iter()
                .map(|name| {
                    Node::new(Kind::Container {
                        name,
                        cluster: cluster.clone(),
                        service: s.clone(),
                        task_role_arn: td.task_role_arn.clone(),
                        tasks: tasks.clone(),
                    })
                })
                .collect()
        }
        Kind::Region { .. } | Kind::Container { .. } | Kind::Task { .. } => vec![],
    })
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else {
        &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "--clipboard", "--input"]]
    };
    let mut last = None;
    for cand in candidates {
        match Command::new(cand[0]).args(&cand[1..]).stdin(Stdio::piped()).spawn() {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(text.as_bytes())?;
                }
                child.wait()?;
                return Ok(());
            }
            Err(e) => last = Some(e),
        }
    }
    Err(anyhow::anyhow!("no clipboard tool found: {:?}", last))
}
