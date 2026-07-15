use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalConfig {
    #[serde(default)]
    pub jira: JiraConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub poll: PollConfig,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
    #[serde(default)]
    pub workflow: WorkflowConfig,
    /// User-editable automation rules. See `crates/jui-core/src/rules.rs`.
    /// v1: rules layer on top of the hardcoded automations (additive); a
    /// later v2 will seed defaults here and remove the hardcoded paths.
    #[serde(default)]
    pub rules: Vec<crate::rules::Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub path: PathBuf,
    #[serde(default)]
    pub nickname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraConfig {
    /// Path to `jira` CLI binary. Defaults to `jira` in PATH.
    #[serde(default)]
    pub binary: Option<String>,
    /// Default JQL filter for "my work" view.
    #[serde(default = "default_my_jql")]
    pub my_jql: String,
    /// Custom-field id for the "reviewer" field. Varies per Jira instance —
    /// `customfield_10015` ("Code Reviewer") is the default on Atlassian Cloud.
    /// Override per site in `~/.config/jui/config.toml`:
    /// `[jira] reviewer_customfield = "customfield_10100"`.
    #[serde(default = "default_reviewer_field")]
    pub reviewer_customfield: String,
    /// Custom-field id for the "DevQA" field. Empty string = feature off
    /// (no auto-write to Jira ticket on PR create, no backfill). Discoverable
    /// via `jira issue meta <key>` or your instance's field admin.
    #[serde(default)]
    pub devqa_customfield: String,
}

impl Default for JiraConfig {
    fn default() -> Self {
        Self {
            binary: None,
            my_jql: default_my_jql(),
            reviewer_customfield: default_reviewer_field(),
            devqa_customfield: String::new(),
        }
    }
}

fn default_my_jql() -> String {
    // No ORDER BY here — jira-cli's `--paginate` flag rejects it. Sorting is requested
    // separately via `--order-by` / `--reverse` in the search call.
    "assignee = currentUser() AND statusCategory != Done".to_string()
}

fn default_reviewer_field() -> String {
    "customfield_10015".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UiConfig {
    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    #[serde(default = "yes")]
    pub on_mention: bool,
    #[serde(default = "yes")]
    pub on_assignment: bool,
    #[serde(default)]
    pub tmux_status: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            on_mention: true,
            on_assignment: true,
            tmux_status: false,
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollConfig {
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self { interval_secs: 120 }
    }
}

fn default_interval() -> u64 {
    120
}

/// Jira workflow states the user considers "in flight". Drives the start/stop
/// hint label and the start_work shortcut. User-editable from the TUI via
/// `Mode::ActiveStatusConfig` so the same binary works for non-MTConnect
/// boards that use different status names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowConfig {
    #[serde(default = "default_active_statuses")]
    pub active_statuses: Vec<String>,
    /// Status to transition newly created tickets into. Empty string disables
    /// the auto-transition (ticket stays at the project's initial status).
    #[serde(default = "default_create_status")]
    pub default_create_status: String,
    /// Status excluded by the "all my tickets" toggle on the List view. The
    /// toggle replaces the default `statusCategory != Done` clause with
    /// `status != "<this>"`, surfacing every assigned ticket except the
    /// terminal state. Empty string = no exclusion.
    #[serde(default = "default_all_mine_exclude_status")]
    pub all_mine_exclude_status: String,
    /// Status to transition a ticket into after a successful PR submission.
    /// Empty string disables the auto-transition (matches the legacy hardcoded
    /// "any 'code review' transition" behavior — kept available for sites
    /// without a custom status).
    #[serde(default = "default_pr_submit_status")]
    pub pr_submit_status: String,
    /// Interactive coding assistant launched from start-work / implementation /
    /// DevQA panes. One of [`CODE_ASSISTANTS`].
    #[serde(default = "default_code_assistant")]
    pub code_assistant: String,
    /// Default `--permission-mode` passed to Claude Code when starting work on
    /// a ticket. One of the values in [`CLAUDE_PERMISSION_MODES`]; an empty
    /// string omits the flag entirely (Claude's built-in default). Editable
    /// from `Mode::Settings`, and overridable per-launch via the plan-mode
    /// toggle in the start-work pane.
    #[serde(default = "default_claude_permission_mode")]
    pub claude_permission_mode: String,
    /// Preferred left-to-right order of Kanban columns, by status name. Columns
    /// whose status appears here are shown first, in this order; any remaining
    /// statuses fall back to the built-in rank ordering. Reordered live with
    /// Shift+←/→ on the Kanban board. Empty = pure built-in ordering.
    #[serde(default)]
    pub kanban_column_order: Vec<String>,
}

/// Valid values for Claude Code's `--permission-mode` argument, in the order
/// shown by the Settings picker. Keep in sync with Claude Code's accepted
/// modes.
pub const CLAUDE_PERMISSION_MODES: &[&str] =
    &["default", "acceptEdits", "plan", "bypassPermissions"];

/// Interactive coding assistants supported by the TUI launch paths.
pub const CODE_ASSISTANTS: &[&str] = &["claude", "opencode"];

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            active_statuses: default_active_statuses(),
            default_create_status: default_create_status(),
            all_mine_exclude_status: default_all_mine_exclude_status(),
            pr_submit_status: default_pr_submit_status(),
            code_assistant: default_code_assistant(),
            claude_permission_mode: default_claude_permission_mode(),
            kanban_column_order: Vec::new(),
        }
    }
}

fn default_code_assistant() -> String {
    "claude".to_string()
}

fn default_claude_permission_mode() -> String {
    "default".to_string()
}

fn default_active_statuses() -> Vec<String> {
    [
        "In Progress",
        "Code Review",
        "Dev QA in Progress",
        "Dev QA Complete",
        "Closed",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_create_status() -> String {
    "Firmware Backlog".to_string()
}

fn default_all_mine_exclude_status() -> String {
    "Firmware Closed".to_string()
}

fn default_pr_submit_status() -> String {
    "Firmware Code Review".to_string()
}

/// Per-repo override loaded from `.jui.toml` if present.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoConfig {
    /// Restrict ticket list to this project key (e.g. "ENG").
    #[serde(default)]
    pub project: Option<String>,
    /// Override JQL for this repo.
    #[serde(default)]
    pub jql: Option<String>,
}

impl GlobalConfig {
    pub fn load() -> Result<Self> {
        let path = crate::paths::config_file()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        crate::paths::ensure_dirs()?;
        let path = crate::paths::config_file()?;
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(&path, raw)?;
        Ok(())
    }

    /// Add a project path (canonicalized) if not already present. Returns true if added.
    pub fn add_project(&mut self, path: PathBuf, nickname: Option<String>) -> bool {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if self.projects.iter().any(|p| p.path == path) {
            return false;
        }
        self.projects.push(ProjectEntry { path, nickname });
        true
    }

    pub fn remove_project(&mut self, path: &Path) -> bool {
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let before = self.projects.len();
        self.projects.retain(|p| p.path != target);
        self.projects.len() != before
    }
}

impl RepoConfig {
    /// Walks upward from `start` looking for `.jui.toml`. Returns (config, dir) if found.
    pub fn discover(start: &Path) -> Result<Option<(Self, PathBuf)>> {
        let mut cur = Some(start);
        while let Some(dir) = cur {
            let candidate = dir.join(".jui.toml");
            if candidate.exists() {
                let raw = std::fs::read_to_string(&candidate)?;
                let cfg: Self = toml::from_str(&raw)
                    .with_context(|| format!("parsing {}", candidate.display()))?;
                return Ok(Some((cfg, dir.to_path_buf())));
            }
            cur = dir.parent();
        }
        Ok(None)
    }
}
