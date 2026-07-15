//! Persistent map of Jira account ids → GitHub handles, used when posting
//! "Reviewer: @<gh>" / "DevQA: @<gh>" Jira comments. Stored as TOML at
//! `~/.config/jui/users.toml` so the user can hand-edit if needed.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct UsersMap {
    /// jira account_id → github handle (no leading @)
    #[serde(default)]
    pub github_handles: HashMap<String, String>,
}

fn config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = PathBuf::from(home).join(".config/jui");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join("users.toml"))
}

impl UsersMap {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        let raw = toml::to_string_pretty(self).context("serializing users map")?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn lookup(&self, account_id: &str) -> Option<&str> {
        self.github_handles.get(account_id).map(|s| s.as_str())
    }

    /// Reverse lookup: find a Jira `account_id` for a given GitHub handle.
    /// Case-insensitive on the handle since GitHub itself folds case.
    pub fn lookup_by_handle(&self, handle: &str) -> Option<&str> {
        let needle = handle.trim_start_matches('@').to_ascii_lowercase();
        self.github_handles
            .iter()
            .find(|(_, h)| h.to_ascii_lowercase() == needle)
            .map(|(id, _)| id.as_str())
    }

    pub fn set(&mut self, account_id: &str, handle: &str) {
        // Strip a leading @ in case the user typed it.
        let h = handle.trim_start_matches('@').to_string();
        self.github_handles.insert(account_id.to_string(), h);
    }
}
