//! User configuration: a TOML file of command templates per tree level.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG: &str = include_str!("../default-config.toml");

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum When {
    ExecDisabled,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub title: String,
    pub command: String,
    #[serde(default)]
    pub when: Option<When>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub ecs: Option<Vec<Action>>,
    #[serde(default)]
    pub cluster: Option<Vec<Action>>,
    #[serde(default)]
    pub service: Option<Vec<Action>>,
    #[serde(default)]
    pub container: Option<Vec<Action>>,
    #[serde(default)]
    pub task: Option<Vec<Action>>,
}

impl Config {
    pub fn defaults() -> Self {
        toml::from_str(DEFAULT_CONFIG).expect("built-in default config is valid")
    }

    /// Default location: $XDG_CONFIG_HOME/ecs-terminal/config.toml, else
    /// ~/.config/ecs-terminal/config.toml on Unix, or the platform config dir on Windows.
    pub fn default_path() -> Option<PathBuf> {
        let base = if cfg!(windows) {
            dirs::config_dir()?
        } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            PathBuf::from(xdg)
        } else {
            dirs::home_dir()?.join(".config")
        };
        Some(base.join("ecs-terminal").join("config.toml"))
    }

    /// Loads the user's config layered over the defaults. A missing file at the
    /// default location is fine; an explicitly passed path must exist.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let mut cfg = Self::defaults();
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => match Self::default_path() {
                Some(p) if p.exists() => p,
                _ => return Ok(cfg),
            },
        };
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading config {}", path.display()))?;
        let user: Config = toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        if let Some(v) = user.ecs {
            cfg.ecs = Some(v);
        }
        if let Some(v) = user.cluster {
            cfg.cluster = Some(v);
        }
        if let Some(v) = user.service {
            cfg.service = Some(v);
        }
        if let Some(v) = user.container {
            cfg.container = Some(v);
        }
        if let Some(v) = user.task {
            cfg.task = Some(v);
        }
        Ok(cfg)
    }
}

/// Replaces `{name}` placeholders with values; unknown placeholders are left as-is.
pub fn render(template: &str, vars: &HashMap<&str, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if after[..end].chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                let key = &after[..end];
                match vars.get(key) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_cover_every_level() {
        let c = Config::defaults();
        for (name, v) in [("ecs", &c.ecs), ("cluster", &c.cluster), ("service", &c.service), ("container", &c.container), ("task", &c.task)] {
            assert!(v.as_ref().map(|v| !v.is_empty()).unwrap_or(false), "{name} has no default actions");
        }
    }

    #[test]
    fn render_replaces_known_and_keeps_unknown() {
        let vars = HashMap::from([("cluster", "c1".to_string()), ("aws", "--profile p".to_string())]);
        assert_eq!(render("aws ecs x --cluster {cluster} {aws} {nope} {bad key}", &vars), "aws ecs x --cluster c1 --profile p {nope} {bad key}");
    }

    #[test]
    fn user_section_replaces_defaults_only_for_that_level() {
        let dir = std::env::temp_dir().join(format!("ecs-terminal-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "[[task]]\ntitle = \"Mine\"\ncommand = \"echo {task}\"\n").unwrap();
        let c = Config::load(Some(&p)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(c.task.as_ref().unwrap().len(), 1);
        assert_eq!(c.task.as_ref().unwrap()[0].title, "Mine");
        assert!(c.service.as_ref().unwrap().len() > 1);
    }
}
