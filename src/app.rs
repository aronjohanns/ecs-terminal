use crate::aws::{self, Ecs, Profile};
use crate::config::{self, Action, Config, When};
use std::collections::HashMap;
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
    pub config: Config,
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
    pub fn new(config: Config, profile: Option<String>, region: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut profiles = aws::load_profiles();
        if let Some(p) = &profile {
            if !profiles.iter().any(|x| &x.name == p) {
                profiles.insert(0, Profile { name: p.clone(), region: None });
            }
        }
        let mut app = App {
            config,
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
        let Some((_, node)) = self.selected_node() else { return };

        let mut vars: HashMap<&str, String> = HashMap::from([
            ("aws", ecs.prefix()),
            ("profile", ecs.profile.clone()),
            ("region", ecs.region.clone()),
        ]);
        let mut exec_disabled = false;
        let put_cluster = |vars: &mut HashMap<&str, String>, c: &aws::Cluster| {
            vars.insert("cluster", c.name.clone());
            vars.insert("cluster_arn", c.arn.clone());
        };
        let put_service = |vars: &mut HashMap<&str, String>, s: &aws::Service| {
            vars.insert("service", s.name.clone());
            vars.insert("service_arn", s.arn.clone());
            vars.insert("task_definition", s.task_definition.clone());
        };

        let actions: &[Action] = match &node.kind {
            Kind::Region { .. } | Kind::Ecs => self.config.ecs.as_deref().unwrap_or_default(),
            Kind::Cluster(c) => {
                put_cluster(&mut vars, c);
                self.config.cluster.as_deref().unwrap_or_default()
            }
            Kind::Service(s) => {
                put_cluster(&mut vars, &aws::Cluster { arn: s.cluster_arn.clone(), name: cluster_name(&s.cluster_arn) });
                put_service(&mut vars, s);
                exec_disabled = !s.enable_execute_command;
                self.config.service.as_deref().unwrap_or_default()
            }
            Kind::Container { name, cluster, service, .. } => {
                put_cluster(&mut vars, cluster);
                put_service(&mut vars, service);
                vars.insert("container", name.clone());
                exec_disabled = !service.enable_execute_command;
                self.config.container.as_deref().unwrap_or_default()
            }
            Kind::Task { task, container, cluster, service } => {
                put_cluster(&mut vars, cluster);
                put_service(&mut vars, service);
                vars.insert("container", container.clone());
                vars.insert("task", task.id().to_string());
                vars.insert("task_arn", task.arn.clone());
                exec_disabled = !service.enable_execute_command;
                self.config.task.as_deref().unwrap_or_default()
            }
        };

        self.commands = actions
            .iter()
            .filter(|a| match a.when {
                Some(When::ExecDisabled) => exec_disabled,
                None => true,
            })
            .map(|a| CliCommand { title: a.title.clone(), command: config::render(&a.command, &vars) })
            .collect();
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
