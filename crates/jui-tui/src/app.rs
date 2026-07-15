use crate::ui;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use jui_core::ipc::{self, ProjectStatus, Request, Response, StartWorkReply, TicketProjectEntry};
use jui_core::jira_api::TransitionOption;
use jui_core::scm::RepoEntry;
use jui_core::ticket::{build_reply_body, fmt_date, priority_rank, Comment, Ticket};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Default issue type when creating a child of `parent`. Jira (classic) refuses to
/// nest sub-tasks under sub-tasks — so when the parent is already a Sub-task, fall
/// back to "Task". The user can still tab to the type field and change it.
fn default_child_type(parent: &Ticket) -> &'static str {
    let pt = parent.issue_type.as_deref().unwrap_or("");
    if pt.eq_ignore_ascii_case("sub-task") || pt.eq_ignore_ascii_case("subtask") {
        "Task"
    } else {
        "Sub-task"
    }
}

fn trim_to_opt(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Parse `(repo, pr_number)` from a GitHub PR web URL like
/// `https://github.com/owner/repo/pull/123`. Returns None when it doesn't look
/// like one. Used to recover PR identity from the standalone `detail_pr_link`
/// when the PR has no cached comments (so `pr_comments` is empty).
fn parse_pr_url(url: &str) -> Option<(String, u64)> {
    let (left, right) = url.split_once("/pull/")?;
    let repo = left
        .rsplit_once("github.com/")
        .map(|(_, r)| r)?
        .trim_matches('/');
    if !repo.contains('/') {
        return None;
    }
    let digits: String = right.chars().take_while(|c| c.is_ascii_digit()).collect();
    let number = digits.parse::<u64>().ok()?;
    Some((repo.to_string(), number))
}

/// Shell commands to run in a freshly-created worktree before launching Claude,
/// extracted from a `WORKTREE_SETUP.md` in the worktree's parent dir (the
/// `<repo>/worktrees/` root). We concatenate the contents of every ```bash /
/// ```sh fenced block, in order. Returns None when there's no such file or no
/// fenced shell blocks — repos without the convention are simply unaffected.
fn worktree_setup_script(worktree: &std::path::Path) -> Option<String> {
    let md_path = worktree.parent()?.join("WORKTREE_SETUP.md");
    let md = std::fs::read_to_string(&md_path).ok()?;
    let mut script = String::new();
    let mut in_block = false;
    for line in md.lines() {
        let trimmed = line.trim_start();
        if !in_block {
            if trimmed.starts_with("```bash") || trimmed.starts_with("```sh") {
                in_block = true;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            in_block = false;
            continue;
        }
        script.push_str(line);
        script.push('\n');
    }
    let script = script.trim();
    (!script.is_empty()).then(|| script.to_string())
}

fn build_claude_context(t: &Ticket, projects: &[std::path::PathBuf], suggestion: &str) -> String {
    let projects_block = if projects.is_empty() {
        "—".to_string()
    } else {
        projects
            .iter()
            .map(|p| format!("- {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "I'm working on a Jira ticket. Here's the context:\n\
\n\
## Ticket: {key}\n\
- **Type:** {issue_type}\n\
- **Status:** {status}\n\
- **Priority:** {priority}\n\
- **Summary:** {summary}\n\
\n\
### Description\n{description}\n\
\n\
## Linked projects\n{projects}\n\
\n\
## Prior implementation suggestion\n{suggestion}\n\
\n\
---\n\
Please help me implement this. You're now in the project's working directory.",
        key = t.key,
        issue_type = t.issue_type.as_deref().unwrap_or("?"),
        status = t.status,
        priority = t.priority.as_deref().unwrap_or("?"),
        summary = t.summary,
        description = t.description.as_deref().unwrap_or("(none)"),
        projects = projects_block,
        suggestion = if suggestion.is_empty() {
            "(none yet)"
        } else {
            suggestion
        },
    )
}

fn code_assistant_label(value: &str) -> &'static str {
    if value.eq_ignore_ascii_case("opencode") {
        "opencode"
    } else {
        "claude"
    }
}

fn assistant_uses_jui_session(value: &str) -> bool {
    matches!(code_assistant_label(value), "claude")
}

fn branch_slug_needs_shortening(slug: &str) -> bool {
    slug.matches('-').count() > 2 || slug.split('-').filter(|s| !s.is_empty()).count() > 3
}

fn fallback_short_branch_slug(t: &Ticket) -> String {
    let key = t.key.to_ascii_uppercase();
    let summary_slug = jui_core::ticket::slugify(&t.summary);
    let mut words: Vec<&str> = summary_slug
        .split('-')
        .filter(|w| !w.is_empty() && *w != "and" && *w != "the" && *w != "for")
        .take(2)
        .collect();
    if words.is_empty() {
        words.push("work");
    }
    while words.len() < 2 {
        words.push("ticket");
    }
    format!("{}-{}-{}", key, words[0], words[1])
}

fn normalize_branch_slug_for_submit(ticket_key: &str, raw: &str) -> String {
    let mut slug = jui_core::ticket::slugify(raw);
    let upper_key = ticket_key.to_ascii_uppercase();
    let lower_key = upper_key.to_ascii_lowercase();
    let compact_upper_key = jui_core::ticket::normalized_ticket_key_upper(ticket_key);
    let compact_lower_key = compact_upper_key.to_ascii_lowercase();
    if !upper_key.is_empty() && slug.starts_with(&lower_key) {
        slug.replace_range(0..lower_key.len(), &upper_key);
    } else if !compact_upper_key.is_empty() && slug.starts_with(&compact_lower_key) {
        slug.replace_range(0..compact_lower_key.len(), &upper_key);
    }
    slug
}

fn code_assistant_cmd(value: &str, context_path: Option<&std::path::Path>) -> String {
    let assistant = code_assistant_label(value);
    if assistant == "opencode" {
        if let Some(path) = context_path {
            let prompt = format!("\"$(cat {})\"", shell_escape(&path.display().to_string()));
            format!("opencode --prompt {prompt}")
        } else {
            "opencode".to_string()
        }
    } else if let Some(path) = context_path {
        format!("cat {} | claude", shell_escape(&path.display().to_string()))
    } else {
        "claude".to_string()
    }
}

fn code_assistant_session_arg(assistant: &str, session_id: &str, resume: bool) -> String {
    match code_assistant_label(assistant) {
        "claude" if resume => format!("--resume {}", shell_escape(session_id)),
        "claude" => format!("--session-id {}", shell_escape(session_id)),
        "opencode" => String::new(),
        _ => String::new(),
    }
}

fn code_assistant_launch_cmd(
    assistant: &str,
    session_arg: &str,
    context_path: Option<&std::path::Path>,
    claude_extra_arg: &str,
) -> String {
    match code_assistant_label(assistant) {
        "claude" => {
            let args = format!("{}{}", session_arg, claude_extra_arg);
            if let Some(path) = context_path {
                format!(
                    "cat {} | claude {args}",
                    shell_escape(&path.display().to_string())
                )
            } else {
                format!("claude {args}")
            }
        }
        "opencode" => {
            if let Some(path) = context_path {
                format!(
                    "opencode {session_arg} --prompt \"$(cat {})\"",
                    shell_escape(&path.display().to_string())
                )
            } else {
                format!("opencode {session_arg}")
            }
        }
        _ => code_assistant_cmd(assistant, context_path),
    }
}

/// Single-quote shell escape — wraps the whole arg in '...' and replaces inner ' with '\''.
fn shell_escape(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Width of the current tmux window in cells. None when not inside tmux or on error.
fn tmux_window_width() -> Option<u16> {
    let out = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{window_width}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

pub enum Mode {
    List,
    Archive,
    Kanban,
    KanbanFilter(KanbanFilterForm),
    Detail,
    Create(CreateForm),
    Edit(EditForm),
    Comment(CommentForm),
    Transition(TransitionForm),
    EditTime(EditTimeForm),
    EditPriority(EditPriorityForm),
    StartWorkPrompt(StartWorkPromptForm),
    DevQaPrompt(DevQaPromptForm),
    DevQaResolveConfirm(DevQaResolveForm),
    DevQaCleanupConfirm(DevQaCleanupForm),
    Implementation(ImplementationForm),
    Projects(ProjectsForm),
    PullRequests(PullRequestsForm),
    CopilotFixRun(CopilotFixForm),
    ProjectsAdd(ProjectsAddForm),
    TicketProjects(TicketProjectsForm),
    ConfluenceSpaces(ConfluenceSpacesForm),
    ConfluencePages(ConfluencePagesForm),
    PageView(PageViewForm),
    Tree(TreeForm),
    AssignPicker(AssignPickerForm),
    /// Modal "are you sure?" confirmation before archiving a ticket. Archiving
    /// transitions the ticket to a closed state (Won't Do / Cancelled / Closed /
    /// Done — daemon picks the first available). Replaces an earlier delete flow
    /// that was rejected by Jira with HTTP 403 on most accounts.
    ArchiveConfirm(ArchiveConfirmForm),
    TicketOptions(TicketOptionsForm),
    /// Open a GitHub PR for the current ticket: title + body + Reviewer + DevQA
    /// pickers + a final `gh pr create` step that comments back on the Jira
    /// ticket and transitions it to Code Review.
    PrCreate(PrCreateForm),
    /// User-editable list of Jira statuses considered "in flight" (the set
    /// that drives the start/stop hint label). Persists to
    /// `GlobalConfig.workflow.active_statuses`.
    ActiveStatusConfig(ActiveStatusForm),
    /// General settings page: workflow defaults the user can tweak without
    /// editing `config.toml` by hand. Opens with `,` from the List view.
    Settings(SettingsForm),
    /// Compose a reply to a GitHub PR comment. Opened with `r` from the
    /// PR Comments pane in Detail view.
    PrCommentReply(PrCommentReplyForm),
    /// Rules engine list. Each row is one `jui_core::rules::Rule`. Editing
    /// happens in `Mode::RuleEdit`. Opens with `:` from the List view.
    Rules(RulesForm),
    RuleEdit(RuleEditForm),
    /// Rules-engine fire history (5-day rolling). Opened with `l` from the
    /// rules list. Newest-first list of action-level log rows.
    RuleLog(RuleLogForm),
    /// Top-level landing pane. Shortcuts to every other view + a recent-
    /// activity feed pulled via `Request::RecentActivity`.
    Home(HomeForm),
}

pub struct HomeForm {
    pub items: Vec<jui_core::cache::ActivityEntry>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Which shortcut row is highlighted in the menu column. Independent of
    /// `selected` (which navigates the activity feed). Tab swaps focus.
    pub menu_selected: usize,
    pub focus: HomeFocus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeFocus {
    Menu,
    Activity,
}

/// Top-level views the home pane can launch. Order is the menu display
/// order; numeric shortcuts 1..n track the same order.
#[derive(Debug, Clone, Copy)]
pub enum HomeTarget {
    List,
    Tree,
    Kanban,
    Archive,
    PullRequests,
    Confluence,
    Settings,
    Rules,
    Projects,
}

pub const HOME_TARGETS: &[(HomeTarget, &str, &str, &str)] = &[
    (
        HomeTarget::List,
        "1",
        "L",
        "Tickets list — assigned + mentions",
    ),
    (
        HomeTarget::Tree,
        "2",
        "T",
        "Tree view — parent → child hierarchy",
    ),
    (
        HomeTarget::Kanban,
        "3",
        "K",
        "Kanban board — columns by status",
    ),
    (
        HomeTarget::Archive,
        "4",
        "A",
        "Archive — closed / cancelled tickets",
    ),
    (
        HomeTarget::PullRequests,
        "5",
        "U",
        "Unmerged PRs — authored open PRs",
    ),
    (
        HomeTarget::Confluence,
        "6",
        "C",
        "Confluence — spaces + pages",
    ),
    (
        HomeTarget::Settings,
        "7",
        ",",
        "Settings — workflow + defaults",
    ),
    (
        HomeTarget::Rules,
        "8",
        ":",
        "Rules engine — automations + log",
    ),
    (
        HomeTarget::Projects,
        "9",
        "P",
        "Projects — manage linked repos",
    ),
];

pub struct RuleLogForm {
    pub items: Vec<jui_core::cache::RuleLogEntry>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

/// List view for the rules engine.
pub struct RulesForm {
    pub items: Vec<jui_core::rules::Rule>,
    pub selected: usize,
    /// Two-press delete guard — index awaiting confirmation.
    pub pending_remove: Option<usize>,
}

/// Single-rule editor. `original_id = None` means "new rule, push on save".
pub struct RuleEditForm {
    pub original_id: Option<String>,
    pub rule: jui_core::rules::Rule,
    /// Which logical row the cursor is on. Layout is dynamic: header rows
    /// (name, trigger, trigger filter, enabled) are followed by one row per
    /// condition then one row per action.
    pub selected_row: usize,
    /// Set when the user is text-editing the focused row's value.
    pub edit_buffer: Option<String>,
    /// What the edit buffer applies to. Mirrors `selected_row` at the time
    /// edit started; refreshed each commit.
    pub edit_target: Option<RuleEditTarget>,
    pub pending_remove_condition: Option<usize>,
    pub pending_remove_action: Option<usize>,
    pub error: Option<String>,
    /// Status picker overlay — open when the user is editing a status-valued
    /// field (currently `Action::JiraTransition.to`). Loaded asynchronously
    /// from `Request::ListStatuses`; `loading=true` while in flight.
    pub picker: Option<RulePicker>,
    /// Variable autocomplete popup. Opens when the user types `{` inside a
    /// text-edit buffer; closes on Enter/Tab (insert), Esc (cancel), or
    /// Backspace past the `{` anchor.
    pub var_picker: Option<VarPicker>,
}

/// Autocomplete state for `{placeholder}` variable insertion inside a
/// rule-edit text buffer. `anchor` is the byte offset of the opening `{`
/// inside the active `edit_buffer`; the filter text is everything between
/// `anchor + 1` and the buffer's current end.
pub struct VarPicker {
    pub anchor: usize,
    pub selected: usize,
}

/// Static list of `(name, description)` for every placeholder the rules
/// engine knows how to substitute. Keep in sync with
/// `jui_core::rules::RuleContext::placeholder_value`.
pub const RULE_VARS: &[(&str, &str)] = &[
    ("ticket_key", "Jira ticket key (e.g. ENG-1234)"),
    ("ticket_summary", "Ticket title"),
    ("ticket_status", "Current ticket status"),
    (
        "project_key",
        "Project prefix from the ticket key (e.g. ENG)",
    ),
    ("issue_type", "Issue type (Story, Bug, …)"),
    ("from_status", "Status before a TicketStatusChanged trigger"),
    ("to_status", "Status after a TicketStatusChanged trigger"),
    ("pr_url", "Full GitHub URL of the linked PR"),
    ("pr_number", "PR number"),
    ("pr_repo", "<owner>/<repo> slug for the linked PR"),
    ("reviewer_handle", "GitHub handle of the picked reviewer"),
    ("devqa_handle", "GitHub handle of the picked DevQA"),
    (
        "reviewer_account_id",
        "Jira account-id of the picked reviewer",
    ),
    ("devqa_account_id", "Jira account-id of the picked DevQA"),
    ("actor", "Display name of the user who triggered the event"),
];

/// Names matching the buffer-tail typed after `{`, case-insensitive. Empty
/// query returns the full list. Returned in stable order so the popup
/// doesn't visually shuffle while the user types.
pub fn filter_vars(query: &str) -> Vec<&'static (&'static str, &'static str)> {
    if query.is_empty() {
        return RULE_VARS.iter().collect();
    }
    let q = query.to_ascii_lowercase();
    RULE_VARS
        .iter()
        .filter(|(name, _)| name.to_ascii_lowercase().contains(&q))
        .collect()
}

#[derive(Debug, Clone)]
pub enum RuleEditTarget {
    Name,
    TriggerFilter, // text field for TicketStatusChanged.to
    ConditionValue(usize),
    ActionValue(usize),
}

/// Status-picker overlay for the rule editor. `target` tells us which field
/// to write the commit back to.
pub struct RulePicker {
    pub target: RulePickerTarget,
    pub query: String,
    pub all: Vec<String>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum RulePickerTarget {
    ActionTransitionTo(usize),
}

impl RulePicker {
    pub fn filtered(&self) -> Vec<&str> {
        if self.query.trim().is_empty() {
            return self.all.iter().map(String::as_str).collect();
        }
        let q = self.query.to_ascii_lowercase();
        self.all
            .iter()
            .filter(|s| s.to_ascii_lowercase().contains(&q))
            .map(String::as_str)
            .collect()
    }
}

/// Editable settings rows. Each row maps to one field on `WorkflowConfig`.
/// Selection moves between rows; `i`/`Enter` opens a status picker populated
/// from Jira; Enter on the picker commits and persists immediately.
pub struct SettingsForm {
    pub selected: usize,
    pub default_create_status: String,
    pub all_mine_exclude_status: String,
    pub pr_submit_status: String,
    /// Interactive coding assistant launched from tmux-backed flows.
    pub code_assistant: String,
    /// Default `--permission-mode` for Claude Code on start-work. Row 3 picks
    /// from the fixed [`jui_core::config::CLAUDE_PERMISSION_MODES`] list rather
    /// than the live Jira statuses used by rows 0–2.
    pub claude_permission_mode: String,
    /// `Some` while the picker overlay is open.
    pub picker: Option<StatusPicker>,
}

/// Status-picker overlay state. Loaded asynchronously: `loading` true while
/// the daemon round-trip is in flight; the user can already type a query.
pub struct StatusPicker {
    /// Which SettingsForm row this picker is editing (0 = default_create,
    /// 1 = all_mine_exclude).
    pub row: usize,
    pub query: String,
    pub all: Vec<String>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

impl StatusPicker {
    /// Lowercased substring filter; case-insensitive. Empty query returns all.
    pub fn filtered(&self) -> Vec<&str> {
        if self.query.trim().is_empty() {
            return self.all.iter().map(String::as_str).collect();
        }
        let q = self.query.to_ascii_lowercase();
        self.all
            .iter()
            .filter(|s| s.to_ascii_lowercase().contains(&q))
            .map(String::as_str)
            .collect()
    }
}

impl SettingsForm {
    pub const ROW_COUNT: usize = 5;
}

pub struct ActiveStatusForm {
    pub items: Vec<String>,
    pub selected: usize,
    /// Two-press delete guard — the index of the row pending removal, or None.
    pub pending_remove: Option<usize>,
    /// When `Some`, the user is typing a new status; on Enter we push and save.
    pub adding: Option<String>,
}

pub struct PrCreateForm {
    pub key: String,
    pub title: String,
    pub body: String,
    /// Reviewer picker state.
    pub reviewer_query: String,
    pub reviewer_results: Vec<(String, String)>,
    pub reviewer: Option<(String, String)>,
    pub reviewer_picker_selected: usize,
    /// DevQA picker state.
    pub devqa_query: String,
    pub devqa_results: Vec<(String, String)>,
    pub devqa: Option<(String, String)>,
    pub devqa_picker_selected: usize,
    /// 0=title, 1=body, 2=reviewer, 3=devqa
    pub field: u8,
    pub busy: bool,
    pub error: Option<String>,
    /// Set when the daemon needs a github handle for a picked Jira user.
    /// While `Some`, the modal collects the handle and submits via
    /// `Request::SetGithubHandle`, then re-tries.
    pub pending_handle: Option<PendingHandle>,
    /// Pre-submit review gate state. Submit (Ctrl-S) starts in `Pending`,
    /// which kicks off a headless Claude `/review` and flips to `Reviewing`.
    /// From `Reviewing`, the user presses `y` to actually fire the PR, `f`
    /// to open an interactive fix-session pane, `R` to re-run the review,
    /// or `Esc` to reset.
    pub review_state: PrReviewState,
    /// Most recent `/review` markdown shown in the modal. `None` while a run
    /// is in flight or before the first run.
    pub review_output: Option<String>,
    /// Vertical scroll offset (in display rows) for the review pane.
    pub review_scroll: usize,
    /// Byte offsets into `title` / `body` for the caret. Maintained on UTF-8
    /// char boundaries — mirrors the Edit-mode pattern.
    pub title_cursor: usize,
    pub body_cursor: usize,
    /// Claude's rewrite of `body`. While `Some`, the modal shows the original
    /// alongside the suggestion with y/n keys to accept or reject.
    pub suggestion: Option<String>,
    /// When set, the user is picking which git remote to push to. Preempts
    /// the normal form keys until they confirm or Esc.
    pub remote_pick: Option<RemotePickerForm>,
    /// Human-readable push/PR route shown in the form.
    pub route_hint: String,
}

#[derive(Debug, Clone)]
pub struct RemotePickerForm {
    /// (remote name, fetch URL)
    pub items: Vec<(String, String)>,
    pub selected: usize,
}

/// Result of resolving which git remote to push to before a PR submit.
pub enum PushRemoteOutcome {
    /// Caller should pass this remote name through to the daemon.
    Use(String),
    /// Picker is now showing on the form; caller must return without
    /// submitting. The user re-presses y after picking.
    PickerOpened,
    /// No remotes found at all (or list failed) — caller continues with
    /// `None`; daemon will fall back to "origin" and surface its own error.
    NoneAvailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrReviewState {
    Pending,
    Reviewing,
}

#[derive(Clone)]
pub struct PendingHandle {
    pub account_id: String,
    pub display_name: String,
    pub handle: String,
}

impl PrCreateForm {
    pub const FIELD_COUNT: u8 = 4;
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeleteOrigin {
    /// Detail Info pane → after archive, return to List.
    DetailInfo,
    /// Detail Subtasks pane → after archive, return to List + refresh.
    Subtasks,
}

pub struct ArchiveConfirmForm {
    pub key: String,
    pub summary: String,
    pub origin: DeleteOrigin,
    /// Populated when the daemon returns an error — modal shows it instead of the
    /// confirmation prompt so the user actually reads it (status bar truncates).
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TicketOptionAction {
    Time,
    Priority,
    Reviewer,
    DevQa,
    ResetPullRequest,
    ClearClaudeSession,
    ClearOpencodeSession,
}

pub struct TicketOptionsForm {
    pub key: String,
    pub selected: usize,
    pub claude_session: Option<String>,
    pub opencode_session: Option<String>,
    pub actions: Vec<TicketOptionAction>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AssignPurpose {
    Assignee,
    Reviewer,
    DevQa,
}

pub struct AssignPickerForm {
    pub key: String,
    pub purpose: AssignPurpose,
    pub query: String,
    /// (display_name, account_id)
    pub results: Vec<(String, String)>,
    pub selected: usize,
    pub error: Option<String>,
}

/// One node in the ticket tree. Stored flat with children referenced by index
/// into `TreeForm.nodes`.
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub key: String,
    pub summary: String,
    pub status: String,
    pub issue_type: Option<String>,
    pub parent_key: Option<String>,
    pub children: Vec<usize>,
    pub depth: u16,
    pub expanded: bool,
    /// True for a ticket the user owns (assigned, reviewer, or mentioned).
    /// False for ancestors fetched purely to give context.
    pub is_mine: bool,
    /// Role badge for the leaf source. Ancestors that are themselves not in any
    /// of the three sets get `None` and render with no badge.
    pub role: Option<MentionRole>,
    /// True when the user has an open PR authored against this ticket. Used
    /// to sort assigned-with-PR rows below assigned-without and to render the
    /// `[PR]` indicator.
    pub has_my_open_pr: bool,
}

#[derive(Clone)]
pub struct TreeForm {
    pub nodes: Vec<TreeNode>,
    pub roots: Vec<usize>,
    /// Flat list of node indices currently visible (after expand/collapse).
    /// Rebuilt by `recompute_visible`.
    pub visible: Vec<usize>,
    pub selected: usize,
    pub two_column: bool,
}

/// One display row in the page viewer.
pub enum PageLine {
    Spans(Vec<ratatui::text::Span<'static>>),
    Blank,
    /// One row inside an image. `id` indexes into PageViewForm.images, `row` is 0..height.
    Image {
        id: usize,
        row: u16,
        height: u16,
    },
}

pub struct PageImage {
    pub proto: ratatui_image::protocol::StatefulProtocol,
}

pub struct PageViewForm {
    pub page_id: String,
    pub space_key: String,
    pub title: String,
    pub markdown: String,
    pub lines: Vec<PageLine>,
    pub images: Vec<PageImage>,
    pub scroll: usize,
    pub viewport_height: usize,
    pub search_active: bool,
    pub search_query: String,
    pub search_matches: Vec<usize>,
    pub search_cursor: usize,
    /// Restore this when user presses q/Esc.
    pub prev_pages: Box<ConfluencePagesForm>,
}

pub struct ConfluenceSpacesForm {
    pub spaces: Vec<jui_core::confluence_api::ConfluenceSpace>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct ConfluencePagesForm {
    pub space_key: String,
    pub space_name: String,
    /// Stack of (page_id, page_title) representing the drill-down path.
    /// Empty = at space root.
    pub breadcrumb: Vec<(String, String)>,
    pub pages: Vec<jui_core::confluence_api::ConfluencePage>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// When true, the search bar is active and results replace the page list.
    pub search_active: bool,
    pub search_query: String,
    pub search_results: Vec<jui_core::confluence_api::ConfluencePage>,
    pub search_selected: usize,
    pub search_loading: bool,
    pub search_error: Option<String>,
    /// True after a search has been submitted; Enter navigates to page instead of re-searching.
    pub search_submitted: bool,
}

pub struct TicketProjectsForm {
    pub ticket_key: String,
    pub items: Vec<TicketProjectEntry>,
    pub selected: usize,
    pub error: Option<String>,
}

pub struct KanbanFilterForm {
    pub query: String,
    /// Individual user results (display_name, account_id).
    pub results: Vec<(String, String)>,
    pub selected: usize,
    /// True when results came from ticket assignees (users table not yet synced).
    pub from_cache: bool,
    /// Saved teams loaded from DB, shown at top of picker.
    pub teams: Vec<ipc::TeamEntry>,
    /// When Some, user is typing a team name to save.
    pub save_name: Option<String>,
}

impl KanbanFilterForm {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            from_cache: false,
            teams: Vec::new(),
            save_name: None,
        }
    }

    /// Total rows in the combined list: teams first, then individual users.
    pub fn total_rows(&self) -> usize {
        self.teams.len() + self.results.len()
    }
}

pub struct ImplementationForm {
    pub key: String,
    /// Markdown body. Empty when nothing's cached yet.
    pub markdown: String,
    pub project_paths: Vec<std::path::PathBuf>,
    pub updated_at: String,
    pub scroll: u16,
    pub status_line: String,
}

pub struct StartWorkPromptForm {
    pub ticket_key: String,
    pub branch_slug: String,
    pub branch_cursor: usize,
    pub time_estimate: String,
    pub priority: String,
    pub need_time: bool,
    pub need_priority: bool,
    /// 0 → location, 1 → branch, 2 → time, 3 → priority. Skipped fields drop out of cycling.
    pub field: u8,
    pub error: Option<String>,
    /// Valid priorities for this Jira instance (live-fetched). Used both as a hint
    /// and to validate the user's input before sending.
    pub valid_priorities: Vec<String>,
    /// Where to land the checkout: worktree (default) or branch in main repo.
    pub location: jui_core::scm::WorkLocation,
    /// Configured default `--permission-mode` (snapshot of
    /// `App::claude_permission_mode` at the time the prompt opened). Used as
    /// the effective mode when `plan_mode` is off.
    pub default_permission_mode: String,
    /// Per-launch plan-mode toggle. When on, Claude Code is launched with
    /// `--permission-mode plan` regardless of the configured default; when
    /// off, the configured default applies. Initialized to true when the
    /// default itself is "plan".
    pub plan_mode: bool,
    /// Whether to also open a bare shell tmux pane in the checkout dir, in
    /// addition to the Claude pane. Off by default: the Claude pane already
    /// lands in the worktree, so this is an opt-in extra working shell.
    pub open_shell_pane: bool,
}

/// Confirmation before resolving DevQA (pressing `P` on a ticket already in
/// "Dev QA In Progress"). Posts "DevQA: Passed" + a 🚀 reaction to the PR, then
/// transitions the ticket forward — all outward-facing, hence the confirm step.
pub struct DevQaResolveForm {
    pub ticket_key: String,
    pub pr_url: String,
    pub error: Option<String>,
}

/// Shown after a resolve when the DevQA worktree has uncommitted changes —
/// confirms the user really wants to discard them and remove the worktree.
pub struct DevQaCleanupForm {
    pub ticket_key: String,
    /// Short summary of what's dirty (e.g. "worktree has 3 uncommitted change(s)").
    pub detail: String,
}

/// Shown right after pressing `Q` (begin DevQA) has transitioned the ticket,
/// to choose where the PR branch gets checked out before Claude launches.
pub struct DevQaPromptForm {
    pub ticket_key: String,
    /// PR url, shown as a hint so the user knows what they're about to test.
    pub pr_url: String,
    /// true → isolate the PR branch in a git worktree (default); false → check
    /// it out in the existing clone (branch-in-repo).
    pub use_worktree: bool,
    pub error: Option<String>,
}

pub struct EditPriorityForm {
    pub key: String,
    pub options: Vec<String>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

pub struct ProjectsForm {
    pub items: Vec<ProjectStatus>,
    pub selected: usize,
    pub pending_remove: Option<PathBuf>,
}

pub struct PullRequestsForm {
    pub items: Vec<jui_core::ipc::PullRequestItem>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

pub struct CopilotFixForm {
    /// Lines above the live tail. Zero means follow the bottom as output arrives.
    pub scroll_from_bottom: usize,
}

#[derive(Clone)]
pub enum CopilotFixStatus {
    Running,
    KillRequested,
    Exited(Option<i32>),
    Failed(String),
}

enum CopilotFixEvent {
    Line(String),
}

pub struct CopilotFixJob {
    pub repo: String,
    pub number: u64,
    pub worktree_path: PathBuf,
    pub command: String,
    pub output: Vec<String>,
    pub status: CopilotFixStatus,
    child: Option<tokio::process::Child>,
    rx: tokio::sync::mpsc::UnboundedReceiver<CopilotFixEvent>,
}

impl CopilotFixJob {
    pub fn is_running(&self) -> bool {
        matches!(
            self.status,
            CopilotFixStatus::Running | CopilotFixStatus::KillRequested
        )
    }
}

fn spawn_copilot_output_reader<R>(
    stream: R,
    tx: tokio::sync::mpsc::UnboundedSender<CopilotFixEvent>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let _ = tx.send(CopilotFixEvent::Line(line));
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(CopilotFixEvent::Line(format!("[output read error: {e}]")));
                    break;
                }
            }
        }
    });
}

pub struct ProjectsAddForm {
    pub query: String,
    pub repos: Vec<RepoEntry>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

impl ProjectsAddForm {
    pub fn filtered(&self) -> Vec<usize> {
        let q = self.query.to_ascii_lowercase();
        if q.is_empty() {
            return (0..self.repos.len()).collect();
        }
        self.repos
            .iter()
            .enumerate()
            .filter(|(_, r)| r.path.to_string_lossy().to_ascii_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }
}

pub struct EditTimeForm {
    pub key: String,
    /// Free-text Jira duration string (e.g. "8h", "2d 4h").
    pub original_estimate: String,
    /// Time to log against the worklog now.
    pub log_work: String,
    pub field: u8, // 0=estimate, 1=log_work
}

#[derive(Clone, Copy)]
pub enum DetailOrigin {
    List,
    Archive,
    Kanban,
}

/// Why a ticket is showing in the bottom List section / has a non-default badge
/// in Tree mode. Order matters: when a ticket would qualify for multiple roles,
/// prefer the more specific one (`Assigned` > `Reviewer` > `Github` > `Mentioned`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MentionRole {
    Assigned,
    Reviewer,
    Github,
    Mentioned,
}

/// User-managed workflow state for a PR the user is reviewing. Stored per
/// ticket-key in SQLite via the daemon. Distinct from GitHub's own PR state
/// (open/closed/merged) — this is "where am I in my review process".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrUserState {
    /// Default — PR has shown up in jui but the user hasn't started yet.
    Awaiting,
    /// User pressed `Q` (begin DevQA) — actively reviewing.
    Reviewing,
    /// User marked done via `K` in Detail.
    Completed,
}

impl PrUserState {
    pub fn from_db(s: &str) -> Self {
        match s {
            "reviewing" => Self::Reviewing,
            "completed" => Self::Completed,
            _ => Self::Awaiting,
        }
    }
    pub fn to_db(&self) -> &'static str {
        match self {
            Self::Awaiting => "awaiting",
            Self::Reviewing => "reviewing",
            Self::Completed => "completed",
        }
    }
}

/// The List view is split into two sections: the user's active tickets at the
/// top and tickets where they're reporter / mentioned at the bottom. Tab in
/// List mode cycles which section receives `j`/`k` and `Enter`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ListFocus {
    Active,
    Mentioned,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DetailFocus {
    Info,
    Projects,
    Subtasks,
    Comments,
    PrComments,
}

impl DetailFocus {
    pub fn next(self) -> Self {
        match self {
            Self::Info => Self::Projects,
            Self::Projects => Self::Subtasks,
            Self::Subtasks => Self::Comments,
            Self::Comments => Self::PrComments,
            Self::PrComments => Self::Info,
        }
    }
    pub fn prev(self) -> Self {
        match self {
            Self::Info => Self::PrComments,
            Self::Projects => Self::Info,
            Self::Subtasks => Self::Projects,
            Self::Comments => Self::Subtasks,
            Self::PrComments => Self::Comments,
        }
    }
}

/// Two-press deletion target. Either a comment id or a linked project path.
/// Ticket deletion uses the modal `Mode::ArchiveConfirm` instead.
pub enum PendingDelete {
    Comment(String),
    Link(std::path::PathBuf),
}

#[derive(Clone)]
pub struct DetailLinkedProject {
    pub project: ProjectStatus,
    /// "confirmed" | "suggested" | "worktree" | "no_match"
    pub state: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Updated,
    Created,
    Status,
    Breadcrumb,
    Project,
    Priority,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::Updated => Self::Created,
            Self::Created => Self::Priority,
            Self::Priority => Self::Status,
            Self::Status => Self::Breadcrumb,
            Self::Breadcrumb => Self::Project,
            Self::Project => Self::Updated,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::Created => "created",
            Self::Status => "status",
            Self::Breadcrumb => "breadcrumb",
            Self::Project => "project",
            Self::Priority => "priority",
        }
    }
}

pub struct CreateForm {
    pub project: String,
    pub issue_type: String,
    pub summary: String,
    pub description: String,
    pub time_estimate: String,
    pub priority: String,
    /// Display name typed by the user. Blank = assign to me on submit.
    pub assignee: String,
    /// Account id (or username) of the picked user. Set when the user selects
    /// from the picker via Enter; used for the post-create `issue assign` call.
    pub assignee_id: Option<String>,
    pub field: u8, // 0=project, 1=type, 2=summary, 3=description, 4=estimate, 5=priority, 6=assignee
    /// Cached results from the last user-search round-trip, shown as a dropdown
    /// when the assignee field has focus.
    pub assignee_results: Vec<(String, String)>, // (display_name, account_id_or_username)
    pub assignee_picker_selected: usize,
    /// When set, the new issue is created as a child of this parent key (usually a
    /// sub-task of the currently-viewed ticket).
    pub parent: Option<String>,
    /// Last submission error, shown inline so the full message is readable (the
    /// status bar truncates).
    pub error: Option<String>,
}

impl CreateForm {
    pub const FIELD_COUNT: u8 = 7;
    pub fn field_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.project,
            1 => &mut self.issue_type,
            2 => &mut self.summary,
            3 => &mut self.description,
            4 => &mut self.time_estimate,
            5 => &mut self.priority,
            _ => &mut self.assignee,
        }
    }
}

pub struct EditForm {
    pub key: String,
    pub summary: String,
    pub description: String,
    pub original_summary: String,
    pub original_description: String,
    /// 0 = summary, 1 = description.
    pub field: u8,
    /// Pending Claude rewrite of `description`. When Some, the UI shows it below the
    /// current body and intercepts y/n to accept/reject.
    pub suggestion: Option<String>,
    /// Byte offset of the caret within `summary` / `description` — one each so
    /// switching panes preserves position. Always sits on a char boundary.
    pub summary_cursor: usize,
    pub description_cursor: usize,
}

fn ticket_edit_file(summary: &str, description: &str) -> String {
    format!("{summary}\n\n{description}")
}

fn parse_ticket_edit_file(raw: &str) -> (String, String) {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = raw.lines();
    let summary = lines.next().unwrap_or_default().trim().to_string();
    let mut description = lines.collect::<Vec<_>>().join("\n");
    if description.starts_with('\n') {
        description.remove(0);
    }
    (summary, description)
}

fn editor_command(editor: &str, path: &std::path::Path) -> String {
    let arg = shell_escape(&path.display().to_string());
    let first = editor
        .split_whitespace()
        .next()
        .and_then(|s| std::path::Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or(editor);
    let has_wait = editor.contains("--wait") || editor.split_whitespace().any(|s| s == "-w");
    let wait = if has_wait {
        ""
    } else if matches!(first, "code" | "code-insiders" | "codium" | "zed" | "atom") {
        " --wait"
    } else if matches!(first, "subl" | "sublime_text" | "mate") {
        " -w"
    } else if matches!(first, "gvim" | "mvim") {
        " -f"
    } else {
        ""
    };
    format!("{editor}{wait} {arg}")
}

/// Caret helpers. Treat the buffer as a flat `&str`; offsets are byte indices
/// that always land on UTF-8 char boundaries.
pub fn edit_left(s: &str, cur: usize) -> usize {
    if cur == 0 {
        return 0;
    }
    let mut new = cur - 1;
    while new > 0 && !s.is_char_boundary(new) {
        new -= 1;
    }
    new
}

pub fn edit_right(s: &str, cur: usize) -> usize {
    if cur >= s.len() {
        return s.len();
    }
    let mut new = cur + 1;
    while new < s.len() && !s.is_char_boundary(new) {
        new += 1;
    }
    new
}

pub fn edit_line_start(s: &str, cur: usize) -> usize {
    s[..cur].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

pub fn edit_line_end(s: &str, cur: usize) -> usize {
    s[cur..].find('\n').map(|i| cur + i).unwrap_or(s.len())
}

fn step_forward_chars(s: &str, mut pos: usize, end: usize, mut chars: usize) -> usize {
    while chars > 0 && pos < end {
        pos += 1;
        while pos < s.len() && !s.is_char_boundary(pos) {
            pos += 1;
        }
        chars -= 1;
    }
    pos
}

pub fn edit_up(s: &str, cur: usize) -> usize {
    let cur_ls = edit_line_start(s, cur);
    if cur_ls == 0 {
        return 0;
    }
    let col = s[cur_ls..cur].chars().count();
    let prev_nl = cur_ls - 1;
    let prev_ls = edit_line_start(s, prev_nl);
    step_forward_chars(s, prev_ls, prev_nl, col)
}

pub fn edit_down(s: &str, cur: usize) -> usize {
    let cur_le = edit_line_end(s, cur);
    if cur_le == s.len() {
        return s.len();
    }
    let cur_ls = edit_line_start(s, cur);
    let col = s[cur_ls..cur].chars().count();
    let next_ls = cur_le + 1;
    let next_le = edit_line_end(s, next_ls);
    step_forward_chars(s, next_ls, next_le, col)
}

pub struct CommentForm {
    pub key: String,
    pub body: String,
    pub reply_to: Option<ReplyContext>,
    /// When true, submitting this form invokes the daemon's `StopWork`
    /// request (which transitions the ticket + appends the comment + fires
    /// the rules engine) instead of the plain `AddComment`. Empty body still
    /// stops the work; the comment is just optional.
    pub from_stop_work: bool,
}

/// Reply form for the GitHub PR Comments pane. Carries the parent comment's
/// kind + id so the daemon can route to the threaded reply endpoint when
/// applicable; falls back to a top-level PR comment otherwise.
pub struct PrCommentReplyForm {
    pub ticket_key: String,
    pub parent_kind: String,
    pub parent_id: String,
    pub parent_author: String,
    pub parent_body: String,
    pub body: String,
    pub body_cursor: usize,
    pub busy: bool,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct ReplyContext {
    pub parent_author: String,
    pub parent_date: String,
    pub parent_body: String,
}

pub struct TransitionForm {
    pub key: String,
    pub options: Vec<TransitionOption>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

pub struct App {
    pub tickets: Vec<Ticket>,
    pub active_idxs: Vec<usize>,
    pub inactive_idxs: Vec<usize>,
    pub ticket_search_active: bool,
    pub ticket_search_query: String,
    pub list_selected: usize,
    pub archive_selected: usize,
    pub mode: Mode,
    pub status: String,
    pub detail: Option<Ticket>,
    pub comments: Vec<Comment>,
    pub comment_selected: usize,
    /// Projects linked to the currently-displayed ticket. Populated when detail loads.
    pub detail_linked_projects: Vec<DetailLinkedProject>,
    pub linked_project_selected: usize,
    pub subtask_selected: usize,
    pub detail_focus: DetailFocus,
    pub detail_origin: DetailOrigin,
    /// accountId of the user, used to check ownership of comments. Loaded once at startup.
    pub my_account_id: Option<String>,
    pub my_display_name: Option<String>,
    /// Tickets where the user is the reviewer (configured custom field).
    pub reviewing_tickets: Vec<Ticket>,
    /// Tickets associated with PRs the user has been requested to review on
    /// GitHub (or @-mentioned on a PR). Sourced via the `gh` CLI.
    pub github_tickets: Vec<Ticket>,
    /// PR comments for the currently-displayed Detail ticket. Empty unless the
    /// ticket is tied to a GitHub PR.
    pub pr_comments: Vec<jui_core::github::PrComment>,
    pub pr_comment_selected: usize,
    /// Canonical PR URL tied to the currently-displayed detail ticket.
    /// `None` when no PR is associated. Populated alongside `pr_comments`.
    pub detail_pr_link: Option<String>,
    /// In-flight Claude "tighten description" task. Set by
    /// `improve_edit_description`, drained by `main_loop` once the daemon
    /// replies — so the UI keeps redrawing (and the user can see the
    /// "asking claude…" indicator) while the request is outstanding.
    pub pending_improve: Option<tokio::sync::oneshot::Receiver<anyhow::Result<String>>>,
    /// In-flight `/review` task spawned by `run_pr_review`. The result either
    /// populates `PrCreateForm.review_output` or surfaces an error in the
    /// status bar. Drained from `main_loop` each tick.
    pub pending_pr_review: Option<tokio::sync::oneshot::Receiver<anyhow::Result<String>>>,
    /// In-flight `improve_pr_body` task. Drained from `main_loop` each tick;
    /// the result populates `PrCreateForm.suggestion`.
    pub pending_pr_body_improve: Option<tokio::sync::oneshot::Receiver<anyhow::Result<String>>>,
    /// Live/background Copilot PR comment fixer launched from Unmerged PRs.
    /// The child process keeps running when the user backs out of the output
    /// screen; the screen can be reopened from the PR pane while it is active.
    pub copilot_fix_job: Option<CopilotFixJob>,
    /// Frame counter advanced once per draw tick (~200 ms). Drives spinner
    /// animation for long-running tasks like `/review` and the body rewrite.
    pub spinner_tick: usize,
    /// Jira statuses the user treats as "in flight" (drives the start/stop
    /// hint label and the start-work shortcut). Loaded from
    /// `GlobalConfig.workflow.active_statuses`; editable in
    /// `Mode::ActiveStatusConfig` and persisted back to the same file.
    pub active_statuses: Vec<String>,
    /// Status applied to newly-created tickets via a post-create transition.
    /// Mirrors `GlobalConfig.workflow.default_create_status`; editable from
    /// `Mode::Settings`.
    pub default_create_status: String,
    /// Status excluded by the `M`-toggle on the main list. Mirrors
    /// `GlobalConfig.workflow.all_mine_exclude_status`; editable from
    /// `Mode::Settings`.
    pub all_mine_exclude_status: String,
    /// Status the daemon transitions a ticket into after a successful PR
    /// submission. Mirrors `GlobalConfig.workflow.pr_submit_status`; editable
    /// from `Mode::Settings`.
    pub pr_submit_status: String,
    /// Default `--permission-mode` passed to Claude Code on "start work".
    /// Mirrors `GlobalConfig.workflow.claude_permission_mode`; editable from
    /// `Mode::Settings` and overridable per-launch in the start-work pane.
    pub claude_permission_mode: String,
    /// Interactive coding assistant launched from start-work / implementation /
    /// DevQA flows. Mirrors `GlobalConfig.workflow.code_assistant`.
    pub code_assistant: String,
    /// Preferred left-to-right Kanban column order by status name. Mirrors
    /// `GlobalConfig.workflow.kanban_column_order`; reordered live with Shift+←/→
    /// on the Kanban board and persisted on each change.
    pub kanban_column_order: Vec<String>,
    /// When true, the next `refresh()` uses an "all my tickets" JQL that drops
    /// the `statusCategory != Done` filter and replaces it with a single
    /// `status != "<all_mine_exclude_status>"` clause. Off by default.
    pub show_all_mine: bool,
    /// User-managed PR review state per ticket key (Awaiting / Reviewing /
    /// Completed). Loaded from the daemon on refresh.
    pub pr_user_states: std::collections::HashMap<String, PrUserState>,
    /// When false, Completed PRs are filtered out of the bottom List section
    /// and Tree mode. Toggle with `K` in the bottom List section.
    pub show_completed_prs: bool,
    /// When false, resolved PR review threads are hidden in the PR Comments
    /// pane. Toggle with `H` from the pane. Default false — resolved threads
    /// are "done" and clutter the view otherwise.
    pub show_resolved_pr_comments: bool,
    /// Tickets where the user has been @-mentioned in Jira text (description /
    /// comments). Disjoint from `reviewing_tickets` and `github_tickets`.
    pub mentioned_tickets: Vec<Ticket>,
    /// Set of ticket keys where the user has an open PR they authored.
    /// Populated by `refresh_mentioned`; drives the `[PR]` indicator and
    /// secondary sort in the tree view.
    pub my_authored_pr_keys: std::collections::HashSet<String>,
    /// Selection index across the **combined** Reviewer + Mentioned list when
    /// the bottom section of the List view is focused.
    pub mentioned_selected: usize,
    /// Which list section in the main view has focus — Shift-Tab cycles.
    pub list_focus: ListFocus,
    /// Pending two-press deletion: a comment id or a linked-project path.
    pub pending_delete: Option<PendingDelete>,
    pub sort_mode: SortMode,
    /// Set of parent ticket keys whose subtasks are currently expanded in the active
    /// list. Tab on a highlighted parent row toggles membership.
    pub expanded_parents: std::collections::HashSet<String>,
    /// Number of non-archived children for each parent key visible in the active list.
    /// Computed during `recompute_indexes` from the pre-collapse set so the value is
    /// stable regardless of current expansion state.
    pub parent_child_counts: std::collections::HashMap<String, usize>,
    /// Depth (0-indexed) of each row in `active_idxs`. Parallel vec; index-aligned.
    pub active_row_depths: Vec<usize>,
    /// Kanban view state: which column is focused and per-column card cursor.
    pub kanban_col: usize,
    pub kanban_card_per_col: Vec<usize>,
    /// Set of display names to show in kanban. Empty = show only current user's tickets.
    pub kanban_assignee_filter: std::collections::HashSet<String>,
    /// Tickets fetched for other users when the assignee filter is active.
    /// Indexed separately from `tickets` so the list view is unaffected.
    pub kanban_extra: Vec<jui_core::ticket::Ticket>,
    /// Set of column indices (into kanban_columns()) that are currently minimized.
    pub kanban_minimized: std::collections::HashSet<usize>,
    /// When Some(ci), that column is expanded to fill the whole board area.
    pub kanban_expanded_col: Option<usize>,
    pub should_quit: bool,
    /// Set after leaving/re-entering alternate screen (e.g. editor launch) so
    /// main_loop clears ratatui's internal buffer before the next draw.
    pub needs_clear: bool,
    /// Image protocol picker. Init lazily before the first PageView open so the
    /// terminal-capability query happens after raw mode is set up.
    pub picker: Option<ratatui_image::picker::Picker>,
    /// '?' overlay showing the current mode's keybindings.
    pub show_help: bool,
    /// When false, the Subtasks pane in Detail view hides children whose status is
    /// in a closed/archived state. Toggle with 'A' from the Subtasks pane.
    pub show_archived_subtasks: bool,
    /// Back-stack of where the user came from. Esc/q pops the top frame so
    /// drilling Tree → Detail or Detail → Subtask → Detail returns to the right
    /// place. Empty stack falls back to `detail_origin`.
    pub nav_stack: Vec<NavFrame>,
}

/// Captured "where to return to" for the back-stack. `Tree` keeps the full
/// form so expand state and selection survive the round-trip; `Detail` also
/// remembers focus + selection so popping back lands on the same sub-pane row.
/// All other variants are markers — reopened via the matching `open_*`
/// helper, losing local state (selection / scroll). Acceptable v1 trade-off.
pub enum NavFrame {
    List,
    Archive,
    Kanban,
    Tree(Box<TreeForm>),
    Detail {
        ticket_key: String,
        focus: DetailFocus,
        subtask_selected: usize,
        comment_selected: usize,
    },
    Home,
    Settings,
    Rules,
    RuleLog,
    /// Stored by id; popping reopens that rule in the editor. Empty string
    /// represents an in-progress "new rule" — we drop those when popping.
    RuleEdit(String),
    Projects,
    PullRequests,
    Confluence,
    ActiveStatusConfig,
}

/// Sentinel offset separating `kanban_extra` indices from `tickets` indices in
/// the kanban column vecs. Values >= this come from `App::kanban_extra`.
const KANBAN_EXTRA_OFFSET: usize = 1 << 24;

impl App {
    async fn get_or_create_assistant_session(
        &self,
        ticket_key: &str,
        assistant: &str,
    ) -> Result<(String, bool)> {
        let assistant = code_assistant_label(assistant);
        let mut s = ipc::connect().await?;
        let existing = match ipc::send_request(
            &mut s,
            &Request::GetAssistantSession {
                ticket_key: ticket_key.to_string(),
                assistant: assistant.to_string(),
            },
        )
        .await?
        {
            Response::AssistantSession { session_id } => session_id,
            _ => None,
        };
        if let Some(id) = existing {
            return Ok((id, true));
        }
        if assistant == "opencode" {
            return Ok((String::new(), false));
        }
        let new_id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| chrono::Utc::now().timestamp_micros().to_string());
        let mut s = ipc::connect().await?;
        let _ = ipc::send_request(
            &mut s,
            &Request::SaveAssistantSession {
                ticket_key: ticket_key.to_string(),
                assistant: assistant.to_string(),
                session_id: new_id.clone(),
            },
        )
        .await?;
        Ok((new_id, false))
    }

    async fn get_assistant_session_value(
        &self,
        ticket_key: &str,
        assistant: &str,
    ) -> Result<Option<String>> {
        let mut s = ipc::connect().await?;
        match ipc::send_request(
            &mut s,
            &Request::GetAssistantSession {
                ticket_key: ticket_key.to_string(),
                assistant: assistant.to_string(),
            },
        )
        .await?
        {
            Response::AssistantSession { session_id } => Ok(session_id),
            Response::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("unexpected response: {other:?}")),
        }
    }

    pub async fn open_ticket_options(&mut self) -> Result<()> {
        let Some(t) = &self.detail else { return Ok(()) };
        let key = t.key.clone();
        let claude_session = self
            .get_assistant_session_value(&key, "claude")
            .await
            .ok()
            .flatten();
        let opencode_session = self
            .get_assistant_session_value(&key, "opencode")
            .await
            .ok()
            .flatten();
        let mut actions = vec![
            TicketOptionAction::Time,
            TicketOptionAction::Priority,
            TicketOptionAction::Reviewer,
            TicketOptionAction::DevQa,
        ];
        if ticket_has_pr(self) {
            actions.push(TicketOptionAction::ResetPullRequest);
        }
        if claude_session.is_some() {
            actions.push(TicketOptionAction::ClearClaudeSession);
        }
        if opencode_session.is_some() {
            actions.push(TicketOptionAction::ClearOpencodeSession);
        }
        self.mode = Mode::TicketOptions(TicketOptionsForm {
            key,
            selected: 0,
            claude_session,
            opencode_session,
            actions,
        });
        Ok(())
    }

    pub async fn submit_ticket_option(&mut self) -> Result<()> {
        let Mode::TicketOptions(form) = &self.mode else {
            return Ok(());
        };
        let Some(action) = form.actions.get(form.selected).copied() else {
            return Ok(());
        };
        let key = form.key.clone();
        match action {
            TicketOptionAction::Time => {
                self.mode = Mode::EditTime(EditTimeForm {
                    key,
                    original_estimate: String::new(),
                    log_work: String::new(),
                    field: 0,
                });
            }
            TicketOptionAction::Priority => {
                self.mode = Mode::Detail;
                self.open_priority_picker().await?;
            }
            TicketOptionAction::Reviewer => {
                self.mode = Mode::AssignPicker(AssignPickerForm {
                    key,
                    purpose: AssignPurpose::Reviewer,
                    query: String::new(),
                    results: vec![],
                    selected: 0,
                    error: None,
                });
            }
            TicketOptionAction::DevQa => {
                self.mode = Mode::AssignPicker(AssignPickerForm {
                    key,
                    purpose: AssignPurpose::DevQa,
                    query: String::new(),
                    results: vec![],
                    selected: 0,
                    error: None,
                });
            }
            TicketOptionAction::ResetPullRequest => {
                let mut s = ipc::connect().await?;
                match ipc::send_request(
                    &mut s,
                    &Request::ResetPullRequest {
                        ticket_key: key.clone(),
                    },
                )
                .await?
                {
                    Response::Ok => {
                        self.status = format!("reset PR for {key}");
                        self.mode = Mode::Detail;
                        self.pr_comments.clear();
                        self.detail_pr_link = None;
                        self.pr_user_states.remove(&key);
                        self.load_detail().await?;
                        self.refresh_list_preserving_status().await?;
                    }
                    Response::Err { message } => self.status = format!("err: {message}"),
                    _ => self.status = "unexpected response".into(),
                }
            }
            TicketOptionAction::ClearClaudeSession | TicketOptionAction::ClearOpencodeSession => {
                let assistant = if action == TicketOptionAction::ClearClaudeSession {
                    "claude"
                } else {
                    "opencode"
                };
                let mut s = ipc::connect().await?;
                match ipc::send_request(
                    &mut s,
                    &Request::ClearAssistantSession {
                        ticket_key: key.clone(),
                        assistant: assistant.to_string(),
                    },
                )
                .await?
                {
                    Response::Ok => {
                        self.status = format!("cleared {assistant} session for {key}");
                        self.open_ticket_options().await?;
                    }
                    Response::Err { message } => self.status = format!("err: {message}"),
                    _ => self.status = "unexpected response".into(),
                }
            }
        }
        Ok(())
    }

    pub fn new() -> Self {
        Self {
            tickets: vec![],
            active_idxs: vec![],
            inactive_idxs: vec![],
            ticket_search_active: false,
            ticket_search_query: String::new(),
            list_selected: 0,
            archive_selected: 0,
            mode: Mode::Home(HomeForm {
                items: Vec::new(),
                selected: 0,
                loading: true,
                error: None,
                menu_selected: 0,
                focus: HomeFocus::Menu,
            }),
            status: "loading…".into(),
            detail: None,
            comments: vec![],
            comment_selected: 0,
            detail_linked_projects: vec![],
            linked_project_selected: 0,
            subtask_selected: 0,
            detail_focus: DetailFocus::Info,
            detail_origin: DetailOrigin::List,
            my_account_id: None,
            my_display_name: None,
            reviewing_tickets: Vec::new(),
            github_tickets: Vec::new(),
            pr_comments: Vec::new(),
            pr_comment_selected: 0,
            detail_pr_link: None,
            pending_improve: None,
            pending_pr_review: None,
            pending_pr_body_improve: None,
            copilot_fix_job: None,
            spinner_tick: 0,
            active_statuses: {
                let cfg = jui_core::config::GlobalConfig::load().unwrap_or_default();
                cfg.workflow.active_statuses
            },
            default_create_status: jui_core::config::GlobalConfig::load()
                .unwrap_or_default()
                .workflow
                .default_create_status,
            all_mine_exclude_status: jui_core::config::GlobalConfig::load()
                .unwrap_or_default()
                .workflow
                .all_mine_exclude_status,
            pr_submit_status: jui_core::config::GlobalConfig::load()
                .unwrap_or_default()
                .workflow
                .pr_submit_status,
            code_assistant: jui_core::config::GlobalConfig::load()
                .unwrap_or_default()
                .workflow
                .code_assistant,
            claude_permission_mode: jui_core::config::GlobalConfig::load()
                .unwrap_or_default()
                .workflow
                .claude_permission_mode,
            kanban_column_order: jui_core::config::GlobalConfig::load()
                .unwrap_or_default()
                .workflow
                .kanban_column_order,
            show_all_mine: false,
            pr_user_states: std::collections::HashMap::new(),
            show_completed_prs: false,
            show_resolved_pr_comments: false,
            mentioned_tickets: Vec::new(),
            my_authored_pr_keys: std::collections::HashSet::new(),
            mentioned_selected: 0,
            list_focus: ListFocus::Active,
            pending_delete: None,
            sort_mode: SortMode::Updated,
            expanded_parents: std::collections::HashSet::new(),
            parent_child_counts: std::collections::HashMap::new(),
            active_row_depths: Vec::new(),
            kanban_col: 0,
            kanban_card_per_col: Vec::new(),
            kanban_assignee_filter: std::collections::HashSet::new(),
            kanban_extra: Vec::new(),
            kanban_minimized: std::collections::HashSet::new(),
            kanban_expanded_col: None,
            should_quit: false,
            needs_clear: false,
            picker: None,
            show_help: false,
            show_archived_subtasks: false,
            nav_stack: Vec::new(),
        }
    }

    /// Look up a ticket by the composite index used in kanban_columns().
    /// Indices >= KANBAN_EXTRA_OFFSET come from `kanban_extra`; others from `tickets`.
    pub fn kanban_ticket(&self, idx: usize) -> Option<&jui_core::ticket::Ticket> {
        if idx >= KANBAN_EXTRA_OFFSET {
            self.kanban_extra.get(idx - KANBAN_EXTRA_OFFSET)
        } else {
            self.tickets.get(idx)
        }
    }

    pub fn ticket_matches_search(&self, t: &Ticket) -> bool {
        let q = self.ticket_search_query.trim().to_ascii_lowercase();
        q.is_empty()
            || t.key.to_ascii_lowercase().contains(&q)
            || t.summary.to_ascii_lowercase().contains(&q)
    }

    pub fn active_search_rows(&self) -> Vec<(usize, usize)> {
        self.active_idxs
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, idx)| {
                self.tickets
                    .get(*idx)
                    .map(|t| self.ticket_matches_search(t))
                    .unwrap_or(false)
            })
            .collect()
    }

    pub fn clamp_list_search_selection(&mut self) {
        let n = self.active_search_rows().len();
        self.list_selected = self.list_selected.min(n.saturating_sub(1));
    }

    pub fn tree_search_visible(&self, form: &TreeForm) -> Vec<usize> {
        let q = self.ticket_search_query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return form.visible.clone();
        }
        form.visible
            .iter()
            .copied()
            .filter(|idx| {
                form.nodes
                    .get(*idx)
                    .map(|n| {
                        n.key.to_ascii_lowercase().contains(&q)
                            || n.summary.to_ascii_lowercase().contains(&q)
                    })
                    .unwrap_or(false)
            })
            .collect()
    }

    pub async fn load_myself(&mut self) -> Result<()> {
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &Request::Myself).await? {
            Response::Myself { info } => {
                self.my_account_id = Some(info.account_id);
                self.my_display_name = Some(info.display_name);
            }
            Response::Err { message } => self.status = format!("auth: {message}"),
            _ => {}
        }
        Ok(())
    }

    /// Lookup the user's review state for a ticket. Default `Awaiting` when
    /// the ticket has never been touched.
    /// Indices into `pr_comments` honouring the `show_resolved_pr_comments`
    /// toggle. Resolved comments are filtered out by default; flipping the
    /// toggle includes them. Selection (`pr_comment_selected`) indexes into
    /// this slice — translate via `visible_pr_comments()[selected]` to get
    /// the underlying `pr_comments[real_idx]`.
    pub fn visible_pr_comments(&self) -> Vec<usize> {
        self.pr_comments
            .iter()
            .enumerate()
            .filter(|(_, c)| self.show_resolved_pr_comments || !c.is_resolved)
            .map(|(i, _)| i)
            .collect()
    }

    /// Number of resolved comments currently hidden from the pane. Used by
    /// the UI to surface a "(N hidden — H to show)" hint when applicable.
    pub fn hidden_pr_comment_count(&self) -> usize {
        if self.show_resolved_pr_comments {
            0
        } else {
            self.pr_comments.iter().filter(|c| c.is_resolved).count()
        }
    }

    /// Spinner glyph for the current frame. Rotates a braille pattern so any
    /// "working…" surface in the UI (review, rewrite, etc.) shows motion.
    pub fn spinner_glyph(&self) -> char {
        const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        FRAMES[self.spinner_tick % FRAMES.len()]
    }

    pub fn pr_state(&self, ticket_key: &str) -> PrUserState {
        self.pr_user_states
            .get(ticket_key)
            .copied()
            .unwrap_or(PrUserState::Awaiting)
    }

    /// Persist a state change via the daemon and update the local cache so
    /// the UI reflects it immediately.
    pub async fn set_pr_state(&mut self, ticket_key: &str, new_state: PrUserState) -> Result<()> {
        let mut s = ipc::connect().await?;
        match ipc::send_request(
            &mut s,
            &Request::SetPrUserState {
                ticket_key: ticket_key.to_string(),
                state: new_state.to_db().to_string(),
            },
        )
        .await?
        {
            Response::Ok => {}
            Response::Err { message } => return Err(anyhow::anyhow!(message)),
            other => return Err(anyhow::anyhow!("unexpected response: {other:?}")),
        }
        self.pr_user_states
            .insert(ticket_key.to_string(), new_state);
        Ok(())
    }

    pub async fn refresh_pr_user_states(&mut self) -> Result<()> {
        let mut s = ipc::connect().await?;
        if let Ok(Response::PrUserStates { items }) =
            ipc::send_request(&mut s, &Request::GetPrUserStates).await
        {
            self.pr_user_states = items
                .into_iter()
                .map(|(k, v)| (k, PrUserState::from_db(&v)))
                .collect();
        }
        Ok(())
    }

    /// Pull tickets where the user is reviewer / @-mentioned (not assigned).
    /// Best-effort — failures only show in the status bar so a refresh of the
    /// main list still goes through.
    pub async fn refresh_mentioned(&mut self) -> Result<()> {
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &Request::ListMyMentions).await? {
            Response::MyMentions {
                reviewing,
                mentioned,
                github,
                authored,
            } => {
                self.reviewing_tickets = reviewing;
                self.github_tickets = github;
                self.mentioned_tickets = mentioned;
                self.my_authored_pr_keys = authored.into_iter().collect();
                let total = self.reviewing_tickets.len()
                    + self.github_tickets.len()
                    + self.mentioned_tickets.len();
                if self.mentioned_selected >= total {
                    self.mentioned_selected = total.saturating_sub(1);
                }
            }
            Response::Err { message } => self.status = format!("mentions: {message}"),
            _ => {}
        }
        Ok(())
    }

    /// Combined reviewer + mentioned list, in the order the bottom List section
    /// renders them (reviewer rows first). `(role, ticket)` tuples.
    pub fn combined_mentions(&self) -> Vec<(MentionRole, &Ticket)> {
        let mut out: Vec<(MentionRole, &Ticket)> = Vec::with_capacity(
            self.reviewing_tickets.len() + self.github_tickets.len() + self.mentioned_tickets.len(),
        );
        let drop_completed = !self.show_completed_prs;
        let keep = |key: &str| -> bool {
            if drop_completed && self.pr_state(key) == PrUserState::Completed {
                return false;
            }
            true
        };
        for t in &self.reviewing_tickets {
            if keep(&t.key) {
                out.push((MentionRole::Reviewer, t));
            }
        }
        for t in &self.github_tickets {
            if keep(&t.key) {
                out.push((MentionRole::Github, t));
            }
        }
        for t in &self.mentioned_tickets {
            if keep(&t.key) {
                out.push((MentionRole::Mentioned, t));
            }
        }
        // Stable sort: rows whose user-managed PR state is Completed sink to
        // the bottom (only meaningful when show_completed_prs is on, since
        // they're filtered out entirely otherwise). Other rows keep their
        // role-grouped order.
        out.sort_by_key(|(_, t)| (self.pr_state(&t.key) == PrUserState::Completed) as u8);
        out
    }

    pub fn comment_is_mine(&self, c: &Comment) -> bool {
        match (&self.my_account_id, &c.account_id) {
            (Some(me), Some(theirs)) => me == theirs,
            _ => false,
        }
    }

    pub async fn delete_selected_comment(&mut self) -> Result<()> {
        let Some(c) = self.comments.get(self.comment_selected) else {
            return Ok(());
        };
        let (Some(id), Some(t)) = (c.id.clone(), self.detail.as_ref().map(|t| t.key.clone()))
        else {
            return Ok(());
        };
        let mut s = ipc::connect().await?;
        match ipc::send_request(
            &mut s,
            &Request::DeleteComment {
                key: t,
                comment_id: id,
            },
        )
        .await?
        {
            Response::Ok => {
                self.status = "comment deleted".into();
                self.load_detail().await?;
            }
            Response::Err { message } => self.status = format!("delete err: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        Ok(())
    }

    /// Rebuild active/inactive index lists from the current `tickets` vec, sorted by the
    /// active SortMode. Active list comes from sort order; inactive list is sorted by
    /// updated DESC regardless (archives always read as "what changed last").
    pub fn recompute_indexes(&mut self) {
        self.active_idxs.clear();
        self.inactive_idxs.clear();
        let mut active: Vec<usize> = Vec::new();
        let mut inactive: Vec<usize> = Vec::new();
        for (i, t) in self.tickets.iter().enumerate() {
            if t.is_inactive() {
                inactive.push(i);
            } else {
                active.push(i);
            }
        }
        let sort = self.sort_mode;
        let cmp = |a: &usize, b: &usize| {
            let ta = &self.tickets[*a];
            let tb = &self.tickets[*b];
            match sort {
                SortMode::Updated => tb.updated.cmp(&ta.updated),
                SortMode::Created => tb.created.cmp(&ta.created),
                SortMode::Status => ta
                    .status
                    .cmp(&tb.status)
                    .then_with(|| tb.updated.cmp(&ta.updated)),
                SortMode::Breadcrumb => {
                    let key_of = |t: &Ticket| -> (String, String, String) {
                        let g = t
                            .grandparent_summary
                            .clone()
                            .or_else(|| t.grandparent_key.clone())
                            .unwrap_or_default();
                        let p = t
                            .parent_summary
                            .clone()
                            .or_else(|| t.parent_key.clone())
                            .unwrap_or_default();
                        let g = if g.is_empty() { "~~~".into() } else { g };
                        let p = if p.is_empty() { "~~~".into() } else { p };
                        (g, p, t.key.clone())
                    };
                    key_of(ta).cmp(&key_of(tb))
                }
                SortMode::Priority => {
                    let key_of = |t: &Ticket| -> (u8, String) {
                        (priority_rank(t.priority.as_deref()), t.key.clone())
                    };
                    key_of(ta).cmp(&key_of(tb))
                }
                SortMode::Project => {
                    // Sort by the basename of the first confirmed linked project
                    // (lowercased, alphabetical). Tickets with no confirmed link sink
                    // to the bottom via a "~~~" sentinel.
                    let key_of = |t: &Ticket| -> (String, String) {
                        let label = t
                            .linked_projects
                            .first()
                            .map(|p| {
                                std::path::Path::new(p)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(p.as_str())
                                    .to_ascii_lowercase()
                            })
                            .unwrap_or_else(|| "~~~".into());
                        (label, t.key.clone())
                    };
                    key_of(ta).cmp(&key_of(tb))
                }
            }
        };
        active.sort_by(cmp);
        inactive.sort_by(|a, b| self.tickets[*b].updated.cmp(&self.tickets[*a].updated));

        // Build hierarchical layout with arbitrary depth, then apply a 3-level sliding
        // window on the deepest expanded chain.
        use std::collections::{HashMap, HashSet};
        let key_to_active_pos: HashMap<&str, usize> = active
            .iter()
            .enumerate()
            .map(|(pos, idx)| (self.tickets[*idx].key.as_str(), pos))
            .collect();
        // children_of[parent_key] = sorted child indices (preserving the active-sort order)
        let mut children_of: HashMap<String, Vec<usize>> = HashMap::new();
        let mut top_level: Vec<usize> = Vec::new();
        for &idx in &active {
            if let Some(pk) = self.tickets[idx].parent_key.clone() {
                if key_to_active_pos.contains_key(pk.as_str()) {
                    children_of.entry(pk).or_default().push(idx);
                    continue;
                }
            }
            top_level.push(idx);
        }
        // Cluster top-level rows by epic so tickets under the same epic appear
        // contiguously and the breadcrumb can dedupe. Epic key = grandparent if
        // present, else parent (covers stories directly under an epic). Empty key
        // = no epic; those keep their original sort position via the same bucket.
        // Bucket order is first-seen, preserving the active-sort comparator.
        {
            let mut bucket_order: Vec<String> = Vec::new();
            let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
            for &idx in &top_level {
                let t = &self.tickets[idx];
                let epic = t
                    .grandparent_key
                    .clone()
                    .or_else(|| t.parent_key.clone())
                    .unwrap_or_default();
                if !buckets.contains_key(&epic) {
                    bucket_order.push(epic.clone());
                }
                buckets.entry(epic).or_default().push(idx);
            }
            top_level = bucket_order
                .into_iter()
                .flat_map(|k| buckets.remove(&k).unwrap_or_default())
                .collect();
        }
        // Snapshot the per-parent count for indicators and Tab gating.
        self.parent_child_counts = children_of
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect();

        // Walk the tree, recording (idx, depth) for every row that should appear
        // when expansion is honored. Tickets, children_of, expanded are read-only here.
        fn walk(
            idx: usize,
            depth: usize,
            out: &mut Vec<(usize, usize)>,
            children_of: &HashMap<String, Vec<usize>>,
            expanded: &HashSet<String>,
            tickets: &[Ticket],
        ) {
            out.push((idx, depth));
            let key = &tickets[idx].key;
            if expanded.contains(key) {
                if let Some(kids) = children_of.get(key) {
                    for &kid in kids {
                        walk(kid, depth + 1, out, children_of, expanded, tickets);
                    }
                }
            }
        }
        let mut rows: Vec<(usize, usize)> = Vec::new();
        for &top in &top_level {
            walk(
                top,
                0,
                &mut rows,
                &children_of,
                &self.expanded_parents,
                &self.tickets,
            );
        }

        // Sliding window: when max depth ≥ 3 (i.e. great-grandchild is visible), hide
        // rows shallower than `max - 2` so we always show three consecutive levels.
        let max_depth = rows.iter().map(|(_, d)| *d).max().unwrap_or(0);
        let min_visible = if max_depth >= 3 { max_depth - 2 } else { 0 };

        let mut visible_idxs: Vec<usize> = Vec::with_capacity(rows.len());
        let mut depths: Vec<usize> = Vec::with_capacity(rows.len());
        for (idx, d) in rows {
            if d >= min_visible {
                visible_idxs.push(idx);
                depths.push(d - min_visible); // re-base so renderer's indent starts at 0
            }
        }
        self.active_idxs = visible_idxs;
        self.active_row_depths = depths;
        self.inactive_idxs = inactive;
        self.list_selected = self
            .list_selected
            .min(self.active_idxs.len().saturating_sub(1));
        self.archive_selected = self
            .archive_selected
            .min(self.inactive_idxs.len().saturating_sub(1));
    }

    /// Returns the ticket currently selected in whichever list view is active.
    pub fn current_ticket(&self) -> Option<&Ticket> {
        let (idxs, sel) = match self.mode {
            Mode::List => match self.list_focus {
                ListFocus::Active
                    if self.ticket_search_active || !self.ticket_search_query.is_empty() =>
                {
                    let rows = self.active_search_rows();
                    let idx = rows.get(self.list_selected).map(|(_, idx)| *idx)?;
                    return self.tickets.get(idx);
                }
                ListFocus::Active => (&self.active_idxs, self.list_selected),
                ListFocus::Mentioned => {
                    // Reviewer → GitHub → Mentioned. Index across all three.
                    let r = self.reviewing_tickets.len();
                    let g = self.github_tickets.len();
                    let i = self.mentioned_selected;
                    return if i < r {
                        self.reviewing_tickets.get(i)
                    } else if i < r + g {
                        self.github_tickets.get(i - r)
                    } else {
                        self.mentioned_tickets.get(i - r - g)
                    };
                }
            },
            Mode::Archive => (&self.inactive_idxs, self.archive_selected),
            Mode::Kanban | Mode::KanbanFilter(_) => {
                let cols = self.kanban_columns();
                let col = cols.get(self.kanban_col)?;
                let card = self
                    .kanban_card_per_col
                    .get(self.kanban_col)
                    .copied()
                    .unwrap_or(0);
                let idx = *col.1.get(card)?;
                return self.kanban_ticket(idx);
            }
            _ => return None,
        };
        idxs.get(sel).and_then(|i| self.tickets.get(*i))
    }

    /// Fetch tickets for all users in `kanban_assignee_filter` and store in
    /// `kanban_extra`. One request per user to avoid JQL `in (...)` compatibility
    /// issues; results are deduplicated by key.
    pub async fn refresh_kanban_for_users(&mut self) -> Result<()> {
        if self.kanban_assignee_filter.is_empty() {
            self.kanban_extra.clear();
            return Ok(());
        }
        let names: Vec<String> = self.kanban_assignee_filter.iter().cloned().collect();
        let mut combined: Vec<jui_core::ticket::Ticket> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut errors: Vec<String> = Vec::new();
        for name in &names {
            let escaped = name.replace('"', "\\\"");
            let jql = format!("assignee = \"{}\" AND statusCategory != Done", escaped);
            let mut s = match ipc::connect().await {
                Ok(s) => s,
                Err(e) => {
                    errors.push(format!("{name}: {e}"));
                    continue;
                }
            };
            match ipc::send_request(
                &mut s,
                &ipc::Request::ListTickets {
                    jql: Some(jql),
                    limit: 100,
                },
            )
            .await
            {
                Ok(ipc::Response::Tickets { items }) => {
                    for t in items {
                        if seen.insert(t.key.clone()) {
                            combined.push(t);
                        }
                    }
                }
                Ok(ipc::Response::Err { message }) => {
                    errors.push(format!("{name}: {message}"));
                }
                Err(e) => {
                    errors.push(format!("{name}: {e}"));
                }
                _ => {}
            }
        }
        // Always write results even when some users failed.
        self.kanban_extra = combined;
        if errors.is_empty() {
            self.status = format!(
                "kanban: {} tickets · {} user(s)",
                self.kanban_extra.len(),
                names.len()
            );
        } else {
            self.status = format!(
                "kanban: {} tickets · errors: {}",
                self.kanban_extra.len(),
                errors.join("; ")
            );
        }
        Ok(())
    }

    /// Group active tickets by status into kanban columns. Returns a vector of
    /// (status_label, ticket_indices) ordered by a typical Jira workflow rank
    /// (To Do → In Progress → Review → QA → Done). Unknown statuses fall to the
    /// end alphabetically.
    pub fn kanban_columns(&self) -> Vec<(String, Vec<usize>)> {
        use std::collections::HashMap;
        fn rank(s: &str) -> u8 {
            match s.to_ascii_lowercase().as_str() {
                "open" | "to do" | "todo" | "backlog" | "active" | "new" => 0,
                "in progress" | "in development" | "in dev" | "doing" => 1,
                "code review" | "review" | "in review" => 2,
                "ready for qa" | "qa ready" => 3,
                "in qa" | "dev qa in progress" | "qa in progress" | "testing" => 4,
                "qa complete" | "dev qa complete" | "qa done" => 5,
                "ready for release" | "ready to deploy" => 6,
                "done" | "closed" | "resolved" => 7,
                _ => 99,
            }
        }
        let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
        // Always include own tickets from the active pool.
        for &i in &self.active_idxs {
            let status = self.tickets[i].status.clone();
            buckets.entry(status).or_default().push(i);
        }
        // When a user filter is active, overlay the extra tickets on top.
        // Indices >= KANBAN_EXTRA_OFFSET come from kanban_extra; see kanban_ticket().
        for i in 0..self.kanban_extra.len() {
            let status = self.kanban_extra[i].status.clone();
            buckets
                .entry(status)
                .or_default()
                .push(KANBAN_EXTRA_OFFSET + i);
        }
        let mut cols: Vec<(String, Vec<usize>)> = buckets.into_iter().collect();
        // Statuses listed in the saved column order come first, in that order;
        // anything else falls back to the built-in rank (then alphabetical).
        let order_pos = |s: &str| {
            self.kanban_column_order
                .iter()
                .position(|x| x.eq_ignore_ascii_case(s))
        };
        cols.sort_by(|a, b| match (order_pos(&a.0), order_pos(&b.0)) {
            (Some(i), Some(j)) => i.cmp(&j),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => rank(&a.0)
                .cmp(&rank(&b.0))
                .then_with(|| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase())),
        });
        cols
    }

    /// Move the selected Kanban column one slot left/right, persisting the new
    /// order to config. Reorders by swapping the two columns' statuses in
    /// `kanban_column_order` (syncing any not-yet-listed visible statuses first),
    /// which flips their relative position regardless of hidden columns between.
    pub fn move_kanban_column(&mut self, right: bool) {
        let cols = self.kanban_columns();
        if cols.is_empty() {
            return;
        }
        let sel = self.kanban_col.min(cols.len() - 1);
        let target = if right {
            if sel + 1 >= cols.len() {
                return;
            }
            sel + 1
        } else {
            if sel == 0 {
                return;
            }
            sel - 1
        };
        let sel_status = cols[sel].0.clone();
        let tgt_status = cols[target].0.clone();
        // Make sure every currently-visible status is in the canonical order so
        // swap-by-position is well defined and the others keep their slots.
        for (status, _) in &cols {
            if !self
                .kanban_column_order
                .iter()
                .any(|x| x.eq_ignore_ascii_case(status))
            {
                self.kanban_column_order.push(status.clone());
            }
        }
        let i = self
            .kanban_column_order
            .iter()
            .position(|x| x.eq_ignore_ascii_case(&sel_status));
        let j = self
            .kanban_column_order
            .iter()
            .position(|x| x.eq_ignore_ascii_case(&tgt_status));
        if let (Some(i), Some(j)) = (i, j) {
            self.kanban_column_order.swap(i, j);
            // Keep the cursor on the moved column and carry its card selection.
            if self.kanban_card_per_col.len() > sel.max(target) {
                self.kanban_card_per_col.swap(sel, target);
            }
            self.kanban_col = target;
            self.kanban_expanded_col = None;
            match self.save_kanban_column_order() {
                Ok(()) => {
                    self.status =
                        format!("moved \"{sel_status}\" {}", if right { "→" } else { "←" })
                }
                Err(e) => self.status = format!("kanban order save err: {e:#}"),
            }
        }
    }

    /// Persist the current Kanban column order to `config.toml`.
    pub fn save_kanban_column_order(&mut self) -> Result<()> {
        let mut cfg = jui_core::config::GlobalConfig::load().unwrap_or_default();
        cfg.workflow.kanban_column_order = self.kanban_column_order.clone();
        cfg.save()?;
        Ok(())
    }

    pub async fn refresh(&mut self) -> Result<()> {
        let mut s = ipc::connect().await?;
        // When the `M`-toggle is on, swap the daemon's default JQL for one that
        // drops the `statusCategory != Done` filter and excludes only the
        // configured terminal status (typically "Firmware Closed"). Empty
        // exclude value falls back to a plain assignee-only JQL.
        let jql = if self.show_all_mine {
            let exclude = self.all_mine_exclude_status.trim();
            if exclude.is_empty() {
                Some("assignee = currentUser()".to_string())
            } else {
                let escaped = exclude.replace('"', "\\\"");
                Some(format!(
                    "assignee = currentUser() AND status != \"{escaped}\""
                ))
            }
        } else {
            None
        };
        match ipc::send_request(&mut s, &Request::ListTickets { jql, limit: 100 }).await? {
            Response::Tickets { items } => {
                // Augment the user's assigned-tickets list with phantom rows for any
                // subtasks they're not assigned to. Each ticket's `subtasks` field
                // (populated from Jira's list response) lists its direct children — we
                // synthesize a stub Ticket per missing child so the tree-fold can show
                // them as expandable rows. Drilling in still works via jira-cli.
                let mut tickets = items;
                let mut known: std::collections::HashSet<String> =
                    tickets.iter().map(|t| t.key.clone()).collect();
                let mut to_add: Vec<Ticket> = Vec::new();
                for t in &tickets {
                    for sub in &t.subtasks {
                        if !known.insert(sub.key.clone()) {
                            continue;
                        }
                        let mut stub = Ticket::new_stub();
                        stub.key = sub.key.clone();
                        stub.summary = sub.summary.clone();
                        stub.status = sub.status.clone().unwrap_or_else(|| "?".into());
                        stub.issue_type = sub.issue_type.clone();
                        stub.parent_key = Some(t.key.clone());
                        to_add.push(stub);
                    }
                }
                tickets.extend(to_add);
                self.tickets = tickets;
                self.recompute_indexes();
                self.status = format!(
                    "{} active / {} archived · sort: {}",
                    self.active_idxs.len(),
                    self.inactive_idxs.len(),
                    self.sort_mode.label(),
                );
            }
            Response::Err { message } => self.status = format!("err: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        // Best-effort refresh of the bottom List section; don't fail the whole
        // refresh if Jira can't answer the mention JQL.
        if let Err(e) = self.refresh_mentioned().await {
            self.status = format!("{} · mentioned err: {e:#}", self.status);
        }
        // PR review state is cheap to pull and gates rendering.
        let _ = self.refresh_pr_user_states().await;
        Ok(())
    }

    async fn refresh_list_preserving_status(&mut self) -> Result<()> {
        let status = self.status.clone();
        self.refresh().await?;
        self.status = status;
        Ok(())
    }

    /// Replace the currently-displayed detail with a different ticket key (used when
    /// drilling into a subtask). Reloads ticket fields, comments, and linked projects.
    pub async fn open_ticket_by_key(&mut self, key: String) -> Result<()> {
        // Stub a Ticket with just the key so load_detail's fallback resolves to it.
        let mut stub = Ticket::new_stub();
        stub.key = key;
        self.detail = Some(stub);
        self.comment_selected = 0;
        self.linked_project_selected = 0;
        self.subtask_selected = 0;
        self.load_detail().await?;
        Ok(())
    }

    pub async fn load_detail(&mut self) -> Result<()> {
        // Pull the key from the active list selection if we're on List/Archive, else from
        // whatever ticket we last loaded into the detail pane. Without this fallback,
        // load_detail would silently no-op whenever called after a sub-mode finishes
        // (submit_comment, esc from TicketProjects, etc.).
        let Some(key) = self
            .current_ticket()
            .map(|t| t.key.clone())
            .or_else(|| self.detail.as_ref().map(|t| t.key.clone()))
        else {
            return Ok(());
        };
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &Request::GetTicket { key: key.clone() }).await? {
            Response::Ticket { ticket: t } => self.detail = Some(t),
            Response::Err { message } => self.status = format!("err: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        // Load comments separately (one extra round-trip; a panel error here shouldn't
        // block the detail view from rendering).
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &Request::ListComments { key: key.clone() }).await? {
            Response::Comments { items } => {
                self.comments = items;
                self.comment_selected = self
                    .comment_selected
                    .min(self.comments.len().saturating_sub(1));
            }
            Response::Err { message } => self.status = format!("comments err: {message}"),
            _ => {}
        }
        // Linked projects (local-only relationship from SQLite).
        let mut s = ipc::connect().await?;
        if let Response::TicketProjects { items } = ipc::send_request(
            &mut s,
            &Request::ListTicketProjects {
                ticket_key: key.clone(),
            },
        )
        .await?
        {
            self.detail_linked_projects = items
                .into_iter()
                .filter(|i| i.linked)
                .map(|i| DetailLinkedProject {
                    project: i.project,
                    state: i.state,
                })
                .collect();
            if self
                .detail_linked_projects
                .iter()
                .any(|p| p.state == "worktree")
            {
                self.detail_linked_projects
                    .retain(|p| p.state == "worktree" || p.state == "confirmed");
            }
            self.linked_project_selected = self
                .linked_project_selected
                .min(self.detail_linked_projects.len().saturating_sub(1));
        }
        // PR comments — only present when the daemon's github-mentions refresh
        // has tied this ticket to a PR. Empty otherwise.
        let mut s = ipc::connect().await?;
        if let Ok(Response::PrComments { items, pr_link }) =
            ipc::send_request(&mut s, &Request::ListPrComments { ticket_key: key }).await
        {
            self.pr_comments = items;
            // Index into the visible-list (resolved filter may hide some).
            let v = self.visible_pr_comments();
            self.pr_comment_selected = self.pr_comment_selected.min(v.len().saturating_sub(1));
            self.detail_pr_link = pr_link;
        } else {
            self.pr_comments.clear();
            self.pr_comment_selected = 0;
            self.detail_pr_link = None;
        }
        Ok(())
    }

    /// Start-work flow: transition to "In Dev", prompt for missing time/priority,
    /// switch SCM branch, then launch Claude Code in tmux (split if window ≥ 400 cols
    /// wide, else new window) cd'd into the top linked project. Captures or resumes
    /// the Claude session id so subsequent starts pick up where you left off.
    pub async fn start_work(&mut self) -> Result<()> {
        // Resolve target ticket: detail view first (we may have just opened it),
        // otherwise the active list selection.
        let ticket = self
            .detail
            .clone()
            .or_else(|| self.current_ticket().cloned());
        let Some(t) = ticket else { return Ok(()) };

        // If work is already in progress on this ticket, treat `s` as "stop" instead.
        if ticket_status_active(&t.status, &self.active_statuses) {
            return self.stop_work().await;
        }

        let need_time = t.original_estimate_seconds.unwrap_or(0) <= 0;
        let need_priority = match t.priority.as_deref() {
            None | Some("") | Some("--") | Some("None") => true,
            _ => false,
        };
        // Always open the prompt so the user can pick worktree vs branch-in-repo,
        // even when time/priority are already filled in.
        let valid_priorities = if need_priority {
            let mut s = ipc::connect().await?;
            match ipc::send_request(&mut s, &Request::ListPriorities).await? {
                Response::Priorities { items } => items,
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let default_branch_slug = t.branch_slug();
        let branch_slug = if branch_slug_needs_shortening(&default_branch_slug) {
            self.status = "shortening branch name…".into();
            match jui_core::claude::short_branch_slug(
                &t,
                &default_branch_slug,
                &self.code_assistant,
            )
            .await
            {
                Ok(slug) => slug,
                Err(_) => fallback_short_branch_slug(&t),
            }
        } else {
            default_branch_slug
        };
        let branch_cursor = branch_slug.len();
        self.mode = Mode::StartWorkPrompt(StartWorkPromptForm {
            ticket_key: t.key.clone(),
            branch_slug,
            branch_cursor,
            time_estimate: String::new(),
            priority: String::new(),
            need_time,
            need_priority,
            field: 0,
            error: None,
            valid_priorities,
            location: jui_core::scm::WorkLocation::Worktree,
            default_permission_mode: self.claude_permission_mode.clone(),
            plan_mode: self.claude_permission_mode.eq_ignore_ascii_case("plan"),
            open_shell_pane: false,
        });
        Ok(())
    }

    /// Move the current ticket back to Backlog and pop a Comment form so the user can
    /// optionally explain why. Esc skips the comment, Ctrl-S submits it.
    pub async fn stop_work(&mut self) -> Result<()> {
        // Pop the Comment form first; submit (or Esc) is what actually sends
        // the StopWork request to the daemon, where the transition + comment
        // + rules-engine fire happen atomically. Esc on the form skips the
        // comment but still stops the work (handled in the key handler).
        let Some(t) = self
            .detail
            .clone()
            .or_else(|| self.current_ticket().cloned())
        else {
            return Ok(());
        };
        self.mode = Mode::Comment(CommentForm {
            key: t.key.clone(),
            body: String::new(),
            reply_to: None,
            from_stop_work: true,
        });
        self.status = format!(
            "stop {}: add optional note then Ctrl-S / Enter, or Esc to skip",
            t.key
        );
        Ok(())
    }

    /// Fire the daemon's `StopWork` with no comment. Called when the user
    /// presses Esc on the stop-work comment form.
    pub async fn submit_stop_work_no_comment(&mut self, key: String) -> Result<()> {
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::StopWork {
                key: key.clone(),
                comment: None,
            },
        )
        .await?;
        match resp {
            Response::Ok => {
                self.status = format!("stopped work on {key}");
                self.refresh_list_preserving_status().await?;
            }
            Response::Err { message } => self.status = format!("stop-work err: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        self.mode = Mode::Detail;
        self.load_detail().await?;
        Ok(())
    }

    pub async fn submit_start_work_prompt(&mut self) -> Result<()> {
        let Mode::StartWorkPrompt(form) = &self.mode else {
            return Ok(());
        };
        let key = form.ticket_key.clone();
        let estimate = if form.need_time {
            trim_to_opt(&form.time_estimate)
        } else {
            None
        };
        let priority = if form.need_priority {
            trim_to_opt(&form.priority)
        } else {
            None
        };
        let location = form.location;
        let open_shell_pane = form.open_shell_pane;
        let branch_slug = normalize_branch_slug_for_submit(&key, &form.branch_slug);
        if branch_slug.is_empty() {
            if let Mode::StartWorkPrompt(f) = &mut self.mode {
                f.error = Some("branch name cannot be empty".into());
            }
            return Ok(());
        }
        // Effective permission mode: plan-toggle wins, else the configured default.
        let permission_mode = if form.plan_mode {
            "plan".to_string()
        } else {
            form.default_permission_mode.clone()
        };

        // Validate priority against the instance's actual list before sending —
        // saves a round-trip and gives a much better error message.
        if let Some(p) = &priority {
            if !form.valid_priorities.is_empty()
                && !form
                    .valid_priorities
                    .iter()
                    .any(|v| v.eq_ignore_ascii_case(p))
            {
                let allowed = form.valid_priorities.join(", ");
                if let Mode::StartWorkPrompt(f) = &mut self.mode {
                    f.error = Some(format!(
                        "'{p}' is not a valid priority on this Jira. Allowed: {allowed}"
                    ));
                }
                return Ok(());
            }
        }

        if let Some(p) = &priority {
            let mut s = ipc::connect().await?;
            let resp = ipc::send_request(
                &mut s,
                &Request::EditPriority {
                    key: key.clone(),
                    priority: p.clone(),
                },
            )
            .await?;
            if let Response::Err { message } = resp {
                if let Mode::StartWorkPrompt(f) = &mut self.mode {
                    f.error = Some(format!("priority: {message}"));
                }
                return Ok(());
            }
        }
        if let Some(e) = &estimate {
            let mut s = ipc::connect().await?;
            let resp = ipc::send_request(
                &mut s,
                &Request::SetEstimate {
                    key: key.clone(),
                    original: Some(e.clone()),
                    remaining: None,
                },
            )
            .await?;
            if let Response::Err { message } = resp {
                if let Mode::StartWorkPrompt(f) = &mut self.mode {
                    f.error = Some(format!("estimate: {message}"));
                }
                return Ok(());
            }
        }
        // Reload ticket so subsequent steps see the new values.
        self.load_detail().await?;
        self.mode = Mode::Detail;
        self.execute_start_work(location, permission_mode, open_shell_pane, branch_slug)
            .await
    }

    async fn execute_start_work(
        &mut self,
        location: jui_core::scm::WorkLocation,
        permission_mode: String,
        open_shell_pane: bool,
        branch_slug: String,
    ) -> Result<()> {
        // Append-only debug log so we can diagnose silent failures of this flow.
        let dbg = |msg: &str| {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/jui-startwork.log")
            {
                let _ = writeln!(f, "{} {msg}", chrono::Utc::now().to_rfc3339());
            }
        };
        dbg(&format!(
            "=== execute_start_work location={:?} ===",
            location
        ));
        let Some(t) = self
            .detail
            .clone()
            .or_else(|| self.current_ticket().cloned())
        else {
            dbg("no ticket in detail/current — bailing");
            self.status = "start-work: no ticket selected".into();
            return Ok(());
        };
        let key = t.key.clone();
        dbg(&format!("ticket key={key} status={:?}", t.status));
        let mut status_parts: Vec<String> = Vec::new();

        // 1. Transition toward the configured in-progress status (best-effort).
        // Jira workflows may require stepping through "Next" states (e.g.
        // Reported → Firmware Active → Firmware In Progress), so the daemon
        // follows Next until a status matching this target is available.
        let target_status = self
            .active_statuses
            .iter()
            .find(|s| s.to_ascii_lowercase().contains("in progress"))
            .cloned()
            .unwrap_or_else(|| "In Progress".to_string());
        let mut s = ipc::connect().await?;
        let transitioned = matches!(
            ipc::send_request(
                &mut s,
                &Request::TransitionToStatus {
                    key: key.clone(),
                    status: target_status.clone(),
                },
            )
            .await,
            Ok(Response::Ok)
        );
        if transitioned {
            status_parts.push(format!("→ {target_status}"));
            self.status = status_parts.join(" · ");
            self.refresh_list_preserving_status().await?;
        }

        // 2. SCM branch switch. Daemon resolves the worktree anchor from the
        // ticket's linked project (cached); `cwd` is only the fallback when
        // the ticket has no linked project on file.
        let cwd = std::env::current_dir()?;
        dbg(&format!(
            "sending StartWork key={key} cwd={} slug={} location={:?}",
            cwd.display(),
            branch_slug,
            location
        ));
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::StartWork {
                key: key.clone(),
                cwd,
                slug: Some(branch_slug),
                location,
            },
        )
        .await?;
        dbg(&format!("StartWork resp={:?}", resp));
        // Path Claude should launch in: worktree dir for worktree mode, repo root for branch-in-repo.
        let mut launch_path: Option<std::path::PathBuf> = None;
        let mut used_branch_in_repo = false;
        match &resp {
            Response::StartWork { reply } => {
                let part = match reply {
                    StartWorkReply::GitWorktree {
                        branch,
                        path,
                        created_branch,
                        attached_existing_worktree,
                    } => {
                        launch_path = Some(path.clone());
                        let action = if *attached_existing_worktree {
                            "reused"
                        } else if *created_branch {
                            "created"
                        } else {
                            "attached"
                        };
                        format!("worktree {action} {branch} → {}", path.display())
                    }
                    StartWorkReply::GitBranchInRepo {
                        branch,
                        path,
                        created_branch,
                        already_on_branch,
                    } => {
                        launch_path = Some(path.clone());
                        used_branch_in_repo = true;
                        let action = if *already_on_branch {
                            "already on"
                        } else if *created_branch {
                            "created"
                        } else {
                            "checked out"
                        };
                        format!("repo {action} {branch} → {}", path.display())
                    }
                    StartWorkReply::SvnExport { value } => format!("svn export {value}"),
                    StartWorkReply::NoScm => "no SCM".into(),
                };
                status_parts.push(part);
            }
            Response::Err { message } => {
                let prior = if status_parts.is_empty() {
                    String::new()
                } else {
                    format!("{} · ", status_parts.join(" · "))
                };
                self.status = format!("{prior}start-work failed: {message}");
                dbg(&format!("daemon err: {message}"));
                return Ok(());
            }
            other => {
                self.status = format!("start-work: unexpected daemon response {other:?}");
                dbg(&format!("unexpected daemon resp: {other:?}"));
                return Ok(());
            }
        }

        // 2b. Optionally open an extra bare shell pane in the checkout dir. The
        // Claude pane (step 8) already lands here, so this is only worth it when
        // the user opted in via the start-work prompt and wants a working shell
        // alongside Claude. Skipped by default to avoid a redundant empty pane.
        if open_shell_pane {
            if let Some(path) = &launch_path {
                if std::env::var("TMUX").is_ok() {
                    let st = std::process::Command::new("tmux")
                        .args(["split-window", "-h", "-c", &path.to_string_lossy()])
                        .status();
                    match st {
                        Ok(s) if s.success() => status_parts.push("shell pane opened".into()),
                        Ok(_) => status_parts.push("tmux split failed".into()),
                        Err(e) => status_parts.push(format!("tmux err: {e}")),
                    }
                } else {
                    status_parts.push("not in tmux — cd manually".into());
                }
            }
        }

        // 3. Find a linked project to cd into. For branch-in-repo mode we cd
        // straight into the repo root (where the branch lives); for worktree mode
        // we keep the existing behavior of preferring the first linked project.
        let project_paths: Vec<std::path::PathBuf> = self
            .detail_linked_projects
            .iter()
            .filter(|p| p.project.available)
            .map(|p| p.project.path.clone())
            .collect();
        dbg(&format!(
            "project_paths={:?} used_branch_in_repo={} launch_path={:?}",
            project_paths, used_branch_in_repo, launch_path
        ));
        // Launch Claude in the checkout dir the daemon resolved: the worktree for
        // worktree mode, the repo root for branch-in-repo. Falling back to the first
        // linked project only when there's no SCM path (NoScm / SvnExport). The old
        // code cd'd into project_paths.first() for worktree mode, which dropped
        // Claude into the main repo on its existing branch instead of the worktree.
        let top = launch_path
            .clone()
            .or_else(|| project_paths.first().cloned());
        let Some(top) = top else {
            self.status = format!(
                "{} · no linked project available — link one (P) and retry",
                status_parts.join(" · ")
            );
            dbg("no top path — bailing");
            return Ok(());
        };
        dbg(&format!("top={}", top.display()));

        let assistant = code_assistant_label(&self.code_assistant);

        // 4. Get or create a per-assistant session id so future starts resume
        // the same conversation for the chosen assistant.
        let (session_id, is_resume) = if assistant_uses_jui_session(assistant) {
            self.get_or_create_assistant_session(&key, assistant)
                .await?
        } else {
            (String::new(), false)
        };
        let session_arg = code_assistant_session_arg(assistant, &session_id, is_resume);

        // 5. Pull the cached implementation suggestion (if any) for the context.
        let mut s = ipc::connect().await?;
        let impl_md = match ipc::send_request(
            &mut s,
            &Request::GetImplementation {
                ticket_key: key.clone(),
            },
        )
        .await?
        {
            Response::Implementation { markdown, .. } => markdown,
            _ => String::new(),
        };

        // Claude `--permission-mode` flag (empty mode = omit, use Claude's default).
        let perm_arg = if permission_mode.trim().is_empty() {
            String::new()
        } else {
            format!(
                " --permission-mode {}",
                shell_escape(permission_mode.trim())
            )
        };

        // 6. Tmux check + width.
        if std::env::var("TMUX").is_err() {
            let run_cmd = code_assistant_launch_cmd(assistant, &session_arg, None, &perm_arg);
            self.status = format!(
                "{} · not in tmux — run: cd {} && {} ({})",
                status_parts.join(" · "),
                top.display(),
                run_cmd,
                if is_resume { "resume" } else { "new" }
            );
            return Ok(());
        }
        let width = tmux_window_width().unwrap_or(0);

        // 7. Build the launch command. Claude resumes already have prior
        // context. Opencode always receives the prompt on process start.
        let resume_without_context = assistant == "claude" && is_resume;
        let cmd = if resume_without_context {
            format!(
                "cd {} && {}",
                shell_escape(&top.display().to_string()),
                code_assistant_launch_cmd(assistant, &session_arg, None, &perm_arg),
            )
        } else {
            let context = build_claude_context(&t, &project_paths, &impl_md);
            let ctx_path = std::env::temp_dir().join(format!("jui-ctx-{}.md", key));
            std::fs::write(&ctx_path, context)?;
            let launch =
                code_assistant_launch_cmd(assistant, &session_arg, Some(&ctx_path), &perm_arg);
            format!(
                "cd {} && {}",
                shell_escape(&top.display().to_string()),
                launch,
            )
        };

        // 8. Tmux split-or-new-window.
        let pane_target = if width >= 400 {
            // Side-by-side split (`-h` in tmux jargon — pane to the right).
            let out = std::process::Command::new("tmux")
                .args(["split-window", "-h", "-P", "-F", "#{pane_id}"])
                .arg(format!("sh -lc {}", shell_escape(&cmd)))
                .output()?;
            if !out.status.success() {
                self.status = format!(
                    "{} · tmux split failed: {}",
                    status_parts.join(" · "),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                return Ok(());
            }
            // Even split.
            let _ = std::process::Command::new("tmux")
                .args(["select-layout", "even-horizontal"])
                .status();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            let out = std::process::Command::new("tmux")
                .args(["new-window", "-P", "-F", "#{pane_id}"])
                .arg(format!("sh -lc {}", shell_escape(&cmd)))
                .output()?;
            if !out.status.success() {
                self.status = format!(
                    "{} · tmux new-window failed: {}",
                    status_parts.join(" · "),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                return Ok(());
            }
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        if assistant == "claude" {
            // After Claude has settled, send `/remote-control` to enable RC mode.
            // Detached subshell so we don't block the TUI.
            let send_cmd = format!(
                "(sleep 4; tmux send-keys -t {pane} '/remote-control' Enter) >/dev/null 2>&1 &",
                pane = pane_target,
            );
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(&send_cmd)
                .spawn();
        } else if assistant == "opencode" {
            if let Ok(jui_bin) = std::env::current_exe() {
                let capture_cmd = format!(
                    "(sleep 60; sid=$(opencode session list | grep -m1 '^ses_' | awk '{{print $1}}'); if [ -n \"$sid\" ]; then {jui} _save-assistant-session {key_arg} opencode \"$sid\"; fi) >/dev/null 2>&1 &",
                    jui = shell_escape(&jui_bin.display().to_string()),
                    key_arg = shell_escape(&key),
                );
                let _ = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&capture_cmd)
                    .spawn();
            }
        }

        let mode = if is_resume { "resumed" } else { "new" };
        let layout = if width >= 400 { "split" } else { "window" };
        self.status = format!(
            "{} · {assistant} {mode} ({layout})",
            status_parts.join(" · "),
        );
        Ok(())
    }

    pub async fn submit_create(&mut self) -> Result<()> {
        let Mode::Create(form) = &self.mode else {
            return Ok(());
        };
        let description = trim_to_opt(&form.description);
        let priority = trim_to_opt(&form.priority);
        let estimate = trim_to_opt(&form.time_estimate);
        let assignee_id_picked = form.assignee_id.clone();
        let assignee_typed = trim_to_opt(&form.assignee);
        let req = Request::CreateTicket {
            project: form.project.clone(),
            issue_type: form.issue_type.clone(),
            summary: form.summary.clone(),
            body: description,
            parent: form.parent.clone(),
        };
        // Clear any previous error before this attempt.
        if let Mode::Create(f) = &mut self.mode {
            f.error = None;
        }
        let mut s = ipc::connect().await?;
        let new_key = match ipc::send_request(&mut s, &req).await? {
            Response::Created { key } => key,
            Response::Err { message } => {
                self.status = "create failed (see form for details)".into();
                if let Mode::Create(f) = &mut self.mode {
                    f.error = Some(message);
                }
                return Ok(());
            }
            _ => {
                if let Mode::Create(f) = &mut self.mode {
                    f.error = Some("unexpected response".into());
                }
                return Ok(());
            }
        };
        // Post-create chained edits — both are best-effort. We surface the failure but
        // the issue itself is already created.
        let mut extras: Vec<&str> = Vec::new();
        if let Some(p) = &priority {
            let mut s = ipc::connect().await?;
            match ipc::send_request(
                &mut s,
                &Request::EditPriority {
                    key: new_key.clone(),
                    priority: p.clone(),
                },
            )
            .await?
            {
                Response::Ok => extras.push("priority"),
                Response::Err { message } => {
                    self.status = format!("created {new_key}, but priority failed: {message}");
                }
                _ => {}
            }
        }
        if let Some(e) = &estimate {
            let mut s = ipc::connect().await?;
            match ipc::send_request(
                &mut s,
                &Request::SetEstimate {
                    key: new_key.clone(),
                    original: Some(e.clone()),
                    remaining: None,
                },
            )
            .await?
            {
                Response::Ok => extras.push("estimate"),
                Response::Err { message } => {
                    self.status = format!("created {new_key}, but estimate failed: {message}");
                }
                _ => {}
            }
        }
        // Assignee — blank means assign to me; non-blank uses the picked id, falling
        // back to the typed text. Skip entirely if neither is available.
        let assignee_arg: Option<String> = match (&assignee_id_picked, assignee_typed.as_deref()) {
            (Some(id), _) if !id.is_empty() => Some(id.clone()),
            (_, Some(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
            _ => self.my_account_id.clone(),
        };
        if let Some(a) = assignee_arg {
            let mut s = ipc::connect().await?;
            match ipc::send_request(
                &mut s,
                &Request::AssignTicket {
                    key: new_key.clone(),
                    assignee: a,
                },
            )
            .await?
            {
                Response::Ok => extras.push("assignee"),
                Response::Err { message } => {
                    self.status = format!("created {new_key}, but assign failed: {message}");
                }
                _ => {}
            }
        }
        // Post-create transition to the configured default status (e.g. "Firmware
        // Backlog"). Best-effort: the new ticket lives in the project's initial
        // state ("Reported") otherwise, which falls outside some default
        // filters and would hide the ticket from the user's list. Empty config
        // value skips this step.
        let target = self.default_create_status.trim().to_string();
        if !target.is_empty() {
            // Route through TransitionToStatus so the daemon's smart picker
            // resolves ambiguous workflows (multiple transitions landing on
            // the same status) the same way the Jira web UI button does.
            let mut s = ipc::connect().await?;
            match ipc::send_request(
                &mut s,
                &Request::TransitionToStatus {
                    key: new_key.clone(),
                    status: target.clone(),
                },
            )
            .await?
            {
                Response::Ok => extras.push("status"),
                Response::Err { message } => {
                    self.status = format!(
                        "created {new_key}, but transition to \"{target}\" failed: {message}"
                    );
                }
                _ => {
                    self.status =
                        format!("created {new_key}, but no transition leads to \"{target}\"");
                }
            }
        }
        if self.status.is_empty() || !self.status.starts_with("created ") {
            self.status = if extras.is_empty() {
                format!("created {new_key}")
            } else {
                format!("created {new_key} (+ {})", extras.join(", "))
            };
        }
        self.mode = Mode::List;
        self.refresh().await?;
        Ok(())
    }

    pub async fn improve_edit_description(&mut self) -> Result<()> {
        let (summary, body) = {
            let Mode::Edit(form) = &self.mode else {
                return Ok(());
            };
            (form.summary.clone(), form.description.clone())
        };
        if body.trim().is_empty() {
            self.status = "nothing to improve — description empty".into();
            return Ok(());
        }
        if self.pending_improve.is_some() {
            self.status = "claude already running — wait…".into();
            return Ok(());
        }
        // Fire-and-forget task. `main_loop` polls the receiver each tick so
        // the UI redraws while we wait — the daemon call can take 10–30s.
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_improve = Some(rx);
        self.status = "asking claude to tighten description…".into();
        tokio::spawn(async move {
            let result: anyhow::Result<String> = async {
                let mut s = ipc::connect().await?;
                let req = Request::ImproveDescription { summary, body };
                match ipc::send_request(&mut s, &req).await? {
                    Response::Improved { body } => Ok(body),
                    Response::Err { message } => Err(anyhow::anyhow!("{message}")),
                    _ => Err(anyhow::anyhow!("unexpected response")),
                }
            }
            .await;
            let _ = tx.send(result);
        });
        Ok(())
    }

    /// Ask Claude to tighten the current PR body. Result lands in
    /// `PrCreateForm.suggestion` for the user to accept/reject. No-op when
    /// not in `PrCreate` or the body is empty.
    pub async fn improve_pr_body(&mut self) -> Result<()> {
        let (ticket_key, title, body) = {
            let Mode::PrCreate(f) = &self.mode else {
                return Ok(());
            };
            (f.key.clone(), f.title.clone(), f.body.clone())
        };
        if body.trim().is_empty() {
            self.status = "nothing to improve — body empty".into();
            return Ok(());
        }
        if self.pending_pr_body_improve.is_some() {
            self.status = "claude already running — wait…".into();
            return Ok(());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_pr_body_improve = Some(rx);
        self.status = "asking claude to tighten PR body (with diff)…".into();
        // Daemon resolves the ticket's worktree, runs `git diff <base>...HEAD`
        // and passes it to Claude alongside title+body so the rewrite reflects
        // the actual change.
        tokio::spawn(async move {
            let result: anyhow::Result<String> = async {
                let mut s = ipc::connect().await?;
                let req = Request::ImprovePrBody {
                    ticket_key,
                    title,
                    body,
                };
                match ipc::send_request(&mut s, &req).await? {
                    Response::Improved { body } => Ok(body),
                    Response::Err { message } => Err(anyhow::anyhow!("{message}")),
                    _ => Err(anyhow::anyhow!("unexpected response")),
                }
            }
            .await;
            let _ = tx.send(result);
        });
        Ok(())
    }

    /// Non-blocking drain for `pending_pr_body_improve`. Called from
    /// `main_loop` each tick. Drops the suggestion onto the form so the
    /// modal can show the y/n prompt, or surfaces an error in the status bar.
    pub fn poll_pending_pr_body_improve(&mut self) -> bool {
        use tokio::sync::oneshot::error::TryRecvError;
        let Some(rx) = self.pending_pr_body_improve.as_mut() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(body)) => {
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.suggestion = Some(body);
                }
                self.status = "claude rewrite ready — y to accept, n to reject".into();
                self.pending_pr_body_improve = None;
                true
            }
            Ok(Err(e)) => {
                self.status = format!("improve failed: {e:#}");
                self.pending_pr_body_improve = None;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Closed) => {
                self.status = "improve task dropped".into();
                self.pending_pr_body_improve = None;
                true
            }
        }
    }

    /// Non-blocking drain for `pending_improve`. Called from `main_loop` each
    /// tick; either applies the suggestion or surfaces an error. Returns
    /// `true` when something changed (so the loop can force-redraw if it cares).
    pub fn poll_pending_improve(&mut self) -> bool {
        use tokio::sync::oneshot::error::TryRecvError;
        let Some(rx) = self.pending_improve.as_mut() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(body)) => {
                if let Mode::Edit(form) = &mut self.mode {
                    form.suggestion = Some(body);
                }
                self.status = "claude rewrite ready — y to accept, n to reject".into();
                self.pending_improve = None;
                true
            }
            Ok(Err(e)) => {
                self.status = format!("err: {e:#}");
                self.pending_improve = None;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Closed) => {
                self.status = "claude task dropped".into();
                self.pending_improve = None;
                true
            }
        }
    }

    pub async fn submit_edit(&mut self) -> Result<()> {
        let Mode::Edit(form) = &self.mode else {
            return Ok(());
        };
        let key = form.key.clone();
        let sum_changed = form.summary != form.original_summary;
        let desc_changed = form.description != form.original_description;
        if !sum_changed && !desc_changed {
            self.mode = Mode::Detail;
            return Ok(());
        }
        let summary = form.summary.clone();
        let description = form.description.clone();

        let mut errs: Vec<String> = Vec::new();
        if sum_changed {
            let mut s = ipc::connect().await?;
            let req = Request::EditSummary {
                key: key.clone(),
                summary: summary.clone(),
            };
            match ipc::send_request(&mut s, &req).await? {
                Response::Ok => {}
                Response::Err { message } => errs.push(format!("summary: {message}")),
                _ => errs.push("summary: unexpected response".into()),
            }
        }
        if desc_changed {
            let mut s = ipc::connect().await?;
            let req = Request::EditDescription {
                key: key.clone(),
                body: description.clone(),
            };
            match ipc::send_request(&mut s, &req).await? {
                Response::Ok => {}
                Response::Err { message } => errs.push(format!("description: {message}")),
                _ => errs.push("description: unexpected response".into()),
            }
        }
        if errs.is_empty() {
            self.status = format!("edited {key}");
            self.mode = Mode::Detail;
            self.load_detail().await?;
        } else {
            self.status = format!("err: {}", errs.join("; "));
        }
        Ok(())
    }

    pub async fn edit_ticket_in_editor(&mut self) -> Result<()> {
        let (key, summary, description) = {
            let Mode::Edit(form) = &self.mode else {
                return Ok(());
            };
            (
                form.key.clone(),
                form.summary.clone(),
                form.description.clone(),
            )
        };
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        let path =
            std::env::temp_dir().join(format!("jui-ticket-{}-{}.md", key, std::process::id()));
        std::fs::write(&path, ticket_edit_file(&summary, &description))?;

        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen)?;
        let cmd = editor_command(&editor, &path);
        let editor_result = std::process::Command::new("sh")
            .arg("-lc")
            .arg(cmd)
            .status();
        let restore_result = (|| -> Result<()> {
            enable_raw_mode()?;
            execute!(io::stdout(), EnterAlternateScreen)?;
            Ok(())
        })();
        self.needs_clear = true;
        restore_result?;

        let status = editor_result?;
        if !status.success() {
            self.status = format!("editor exited with {status}");
            return Ok(());
        }

        let edited = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);
        let (new_summary, new_description) = parse_ticket_edit_file(&edited);
        if new_summary.is_empty() {
            self.status = "summary cannot be empty".into();
            return Ok(());
        }
        if let Mode::Edit(form) = &mut self.mode {
            form.summary = new_summary;
            form.description = new_description;
            form.summary_cursor = form.summary.len();
            form.description_cursor = form.description.len();
            form.suggestion = None;
        }
        self.submit_edit().await
    }

    pub async fn submit_comment(&mut self) -> Result<()> {
        let Mode::Comment(form) = &self.mode else {
            return Ok(());
        };
        let from_stop_work = form.from_stop_work;
        let key = form.key.clone();
        // If this is a reply, prefix the body with the visible quote marker so other Jira
        // clients see the context too, and so jui can detect it on render.
        let body = if let Some(ctx) = &form.reply_to {
            build_reply_body(
                &ctx.parent_author,
                &fmt_date(&ctx.parent_date),
                &ctx.parent_body,
                &form.body,
            )
        } else {
            form.body.clone()
        };
        let req = if from_stop_work {
            // Stop-work flow: send StopWork so daemon does transition +
            // comment + rules-engine fire as one atomic step. Empty body is
            // still valid — daemon skips the comment write.
            let comment = if body.trim().is_empty() {
                None
            } else {
                Some(body)
            };
            Request::StopWork {
                key: key.clone(),
                comment,
            }
        } else {
            Request::AddComment {
                key: key.clone(),
                body,
            }
        };
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &req).await? {
            Response::Ok => {
                self.status = if from_stop_work {
                    format!("stopped work on {key}")
                } else {
                    format!("commented on {key}")
                };
                self.mode = Mode::Detail;
                self.load_detail().await?;
                if from_stop_work {
                    self.refresh_list_preserving_status().await?;
                }
            }
            Response::Err { message } => self.status = format!("err: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        Ok(())
    }

    pub async fn open_transition(&mut self, key: String) -> Result<()> {
        self.mode = Mode::Transition(TransitionForm {
            key: key.clone(),
            options: vec![],
            selected: 0,
            loading: true,
            error: None,
        });
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(&mut s, &Request::ListTransitions { key }).await?;
        if let Mode::Transition(form) = &mut self.mode {
            form.loading = false;
            match resp {
                Response::Transitions { items } => {
                    form.options = items;
                    if form.options.is_empty() {
                        form.error = Some("no transitions available for this issue".into());
                    }
                }
                Response::Err { message } => form.error = Some(message),
                _ => form.error = Some("unexpected response".into()),
            }
        }
        Ok(())
    }

    pub async fn open_implementation(&mut self) -> Result<()> {
        let Some(t) = &self.detail else { return Ok(()) };
        let key = t.key.clone();
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::GetImplementation {
                ticket_key: key.clone(),
            },
        )
        .await?;
        let mut form = ImplementationForm {
            key: key.clone(),
            markdown: String::new(),
            project_paths: vec![],
            updated_at: String::new(),
            scroll: 0,
            status_line: String::new(),
        };
        match resp {
            Response::Implementation {
                markdown,
                project_paths,
                updated_at,
            } => {
                form.markdown = markdown;
                form.project_paths = project_paths
                    .into_iter()
                    .map(std::path::PathBuf::from)
                    .collect();
                form.updated_at = updated_at;
            }
            Response::Err { .. } => {
                // Nothing cached. Trigger generation and show a placeholder.
                let mut s = ipc::connect().await?;
                let _ =
                    ipc::send_request(&mut s, &Request::GenerateImplementation { ticket_key: key })
                        .await?;
                form.status_line =
                    "no cached suggestion — generation queued. press 'r' to reload.".into();
            }
            _ => form.status_line = "unexpected response".into(),
        }
        self.mode = Mode::Implementation(form);
        Ok(())
    }

    pub async fn regenerate_implementation(&mut self) -> Result<()> {
        let Mode::Implementation(form) = &self.mode else {
            return Ok(());
        };
        let key = form.key.clone();
        let mut s = ipc::connect().await?;
        let _ =
            ipc::send_request(&mut s, &Request::GenerateImplementation { ticket_key: key }).await?;
        if let Mode::Implementation(form) = &mut self.mode {
            form.status_line = "generation queued. press 'r' again later to reload.".into();
        }
        Ok(())
    }

    pub async fn reload_implementation(&mut self) -> Result<()> {
        let Mode::Implementation(form) = &self.mode else {
            return Ok(());
        };
        let key = form.key.clone();
        let mut s = ipc::connect().await?;
        let resp =
            ipc::send_request(&mut s, &Request::GetImplementation { ticket_key: key }).await?;
        if let Mode::Implementation(form) = &mut self.mode {
            match resp {
                Response::Implementation {
                    markdown,
                    project_paths,
                    updated_at,
                } => {
                    form.markdown = markdown;
                    form.project_paths = project_paths
                        .into_iter()
                        .map(std::path::PathBuf::from)
                        .collect();
                    form.updated_at = updated_at;
                    form.status_line = format!(
                        "loaded · updated {updated_at}",
                        updated_at = form.updated_at
                    );
                }
                Response::Err { message } => form.status_line = format!("not yet ready: {message}"),
                _ => form.status_line = "unexpected response".into(),
            }
        }
        Ok(())
    }

    pub async fn save_implementation_to_file(&mut self) -> Result<()> {
        let Mode::Implementation(form) = &self.mode else {
            return Ok(());
        };
        if form.markdown.is_empty() {
            self.status = "nothing to save (no implementation yet)".into();
            return Ok(());
        }
        let Some(t) = &self.detail else { return Ok(()) };
        let header = format!(
            "# {key}: {summary}\n\n\
- **Type:** {issue_type}\n\
- **Status:** {status}\n\
- **Priority:** {priority}\n\
- **Assignee:** {assignee}\n\
- **Linked projects:** {projects}\n\
- **Generated:** {updated}\n\n\
---\n\n",
            key = t.key,
            summary = t.summary,
            issue_type = t.issue_type.as_deref().unwrap_or("?"),
            status = t.status,
            priority = t.priority.as_deref().unwrap_or("?"),
            assignee = t.assignee.as_deref().unwrap_or("—"),
            projects = if form.project_paths.is_empty() {
                "—".into()
            } else {
                form.project_paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            updated = form.updated_at,
        );
        let body = format!("{header}{}", form.markdown);
        let cwd = std::env::current_dir()?;
        let path = cwd.join(format!("jui-{}.md", t.key));
        std::fs::write(&path, body)?;
        self.status = format!("saved → {}", path.display());
        if let Mode::Implementation(form) = &mut self.mode {
            form.status_line = format!("saved → {}", path.display());
        }
        Ok(())
    }

    /// PR url tied to the currently-open ticket — from a cached comment, or the
    /// standalone PR link (set even when the PR has no comments). None when the
    /// ticket has no associated PR.
    fn current_pr_url(&self) -> Option<String> {
        self.pr_comments
            .first()
            .map(|c| c.pr_url.clone())
            .or_else(|| self.detail_pr_link.clone())
    }

    fn current_pr_identity(&self) -> Option<(String, String, u64)> {
        if let Some(c) = self.pr_comments.first() {
            return Some((c.repo.clone(), c.pr_url.clone(), c.pr_number));
        }
        self.detail_pr_link
            .as_deref()
            .and_then(parse_pr_url)
            .map(|(repo, number)| {
                (
                    repo,
                    self.detail_pr_link.clone().unwrap_or_default(),
                    number,
                )
            })
    }

    async fn build_devqa_context(
        &self,
        key: &str,
        repo: Option<&str>,
        pr_url: Option<&str>,
        pr_number: Option<u64>,
        branch: Option<&str>,
    ) -> String {
        let mut ctx = String::new();
        if let Some(t) = &self.detail {
            ctx.push_str(&format!("# DevQA: {}\n\n", t.key));
            ctx.push_str("## Jira ticket\n\n");
            ctx.push_str(&format!("- **Status:** {}\n", t.status));
            ctx.push_str(&format!("- **Summary:** {}\n", t.summary));
            if let Some(issue_type) = t.issue_type.as_deref() {
                ctx.push_str(&format!("- **Type:** {issue_type}\n"));
            }
            if let Some(priority) = t.priority.as_deref() {
                ctx.push_str(&format!("- **Priority:** {priority}\n"));
            }
            if let Some(assignee) = t.assignee.as_deref() {
                ctx.push_str(&format!("- **Assignee:** {assignee}\n"));
            }
            if let Some(parent) = t.parent_key.as_deref() {
                ctx.push_str(&format!("- **Parent:** {parent}\n"));
            }
            if let Some(d) = &t.description {
                if !d.trim().is_empty() {
                    ctx.push_str("\n### Jira description\n\n");
                    ctx.push_str(d.trim());
                    ctx.push('\n');
                }
            }
        } else {
            ctx.push_str(&format!("# DevQA: {key}\n\n"));
        }

        ctx.push_str("\n## Pull request\n\n");
        if let Some(url) = pr_url {
            ctx.push_str(&format!("- **URL:** {url}\n"));
        }
        if let Some(repo) = repo {
            ctx.push_str(&format!("- **Repo:** `{repo}`\n"));
        }
        if let Some(number) = pr_number {
            ctx.push_str(&format!("- **PR number:** `{number}`\n"));
        }
        if let Some(branch) = branch {
            ctx.push_str(&format!("- **Local branch:** `{branch}`\n"));
        }

        if let (Some(repo), Some(number)) = (repo, pr_number) {
            if let Ok(author) = jui_core::github::pr_author_login(repo, number).await {
                if !author.trim().is_empty() {
                    ctx.push_str(&format!("- **Author:** @{author}\n"));
                }
            }
            if let Ok(body) = jui_core::github::pr_body(repo, number).await {
                if !body.trim().is_empty() {
                    ctx.push_str("\n### PR request message\n\n");
                    ctx.push_str(body.trim());
                    ctx.push('\n');
                }
            }
        }

        if !self.pr_comments.is_empty() {
            ctx.push_str("\n## PR discussion and reviews\n\n");
            for c in &self.pr_comments {
                let kind = if c.kind.is_empty() {
                    "comment"
                } else {
                    &c.kind
                };
                ctx.push_str(&format!(
                    "### @{author} · {date} · {kind}\n\n{body}\n\n",
                    author = c.author,
                    date = c.created.split('T').next().unwrap_or(&c.created),
                    body = c.body.trim(),
                ));
            }
        }

        ctx.push_str(
            "\n## DevQA instructions\n\n\
You are doing a DevQA pass on this pull request. The PR branch is already checked out in this directory. Your job is to **test and verify the existing change**, not to implement the ticket or write a solution. Run / smoke-test the change as it stands, look for regressions, and report findings to me directly here. Do **not** modify code to 'fix' or 'finish' the ticket; only change files if I explicitly ask you to (e.g. to reproduce or probe a bug).\n\n\
## Rules for posting on the PR\n\n\
- **Never** post a pass / fail / approval comment to GitHub (`gh pr comment`, `gh pr review --approve`, `gh pr review --request-changes`, etc.) without my explicit approval first. Show me the proposed comment text here and wait for me to say go.\n\
- When I do approve and you post the pass/fail comment, also add a rocket reaction to the PR description itself (the top-level body posted by the author, not your own comment).\n",
        );

        ctx
    }

    /// Set up a DevQA checkout for the ticket's PR (daemon side) and open a
    /// tmux pane in it running `claude` with PR context. With `use_worktree` the
    /// PR branch is isolated in a git worktree; otherwise it's checked out in the
    /// existing clone. No-op (and returns `Ok`) when the ticket has no PR cached.
    pub async fn begin_devqa_worktree(&mut self, key: &str, use_worktree: bool) -> Result<()> {
        // PR identity: prefer a cached comment (carries repo + number), else
        // recover it from the standalone PR link, which is set even when the PR
        // has zero comments. Without either there's no PR to DevQA.
        let (repo, pr_url, pr_number) = if let Some(c) = self.pr_comments.first().cloned() {
            (c.repo, c.pr_url, c.pr_number)
        } else if let Some((repo, num)) = self.detail_pr_link.as_deref().and_then(parse_pr_url) {
            (repo, self.detail_pr_link.clone().unwrap_or_default(), num)
        } else {
            self.status = format!("DevQA: no PR associated with {key}");
            return Ok(());
        };

        // 1. Daemon: locate clone, fetch PR head, create worktree or check out.
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::SetupDevQaWorktree {
                ticket_key: key.to_string(),
                repo: repo.clone(),
                pr_number,
                worktree: use_worktree,
            },
        )
        .await?;
        let (path, branch) = match resp {
            Response::DevQaWorktree { path, branch } => (path, branch),
            Response::Err { message } => return Err(anyhow::anyhow!(message)),
            _ => return Err(anyhow::anyhow!("unexpected daemon response")),
        };

        let assistant = code_assistant_label(&self.code_assistant);

        // 2. Tmux check.
        if std::env::var("TMUX").is_err() {
            self.status = format!(
                "DevQA checkout at {} (branch {branch}) — open tmux to launch {assistant}",
                path.display()
            );
            return Ok(());
        }

        // 3. Get/create a session id for assistants with explicit resume
        // support.
        let (session_id, is_resume) = if assistant_uses_jui_session(assistant) {
            self.get_or_create_assistant_session(key, assistant).await?
        } else {
            (String::new(), false)
        };
        let session_arg = code_assistant_session_arg(assistant, &session_id, is_resume);

        // 3b. Worktree setup fixes. For a worktree checkout, run any commands
        // from `<repo>/worktrees/WORKTREE_SETUP.md` (submodule init, shared
        // downloads/sstate symlinks, …) in the worktree before Claude starts.
        // Best-effort and idempotent, so re-running on resume is harmless.
        let setup_prefix = if use_worktree {
            match worktree_setup_script(&path) {
                Some(s) => format!("echo '── applying worktree setup ──'\n{s}\n"),
                None => String::new(),
            }
        } else {
            String::new()
        };
        let pdisp = shell_escape(&path.display().to_string());

        // 4. Build PR context file (ticket info + PR url + cached PR comments).
        let cmd = if assistant == "claude" && is_resume {
            let launch = code_assistant_launch_cmd(assistant, &session_arg, None, "");
            format!("cd {pdisp} || exit 1\n{setup_prefix}{launch}")
        } else {
            let ctx = self
                .build_devqa_context(
                    key,
                    Some(&repo),
                    Some(&pr_url),
                    Some(pr_number),
                    Some(&branch),
                )
                .await;

            let ctx_path = std::env::temp_dir().join(format!("jui-devqa-{key}.md"));
            std::fs::write(&ctx_path, ctx)?;
            let launch = code_assistant_launch_cmd(assistant, &session_arg, Some(&ctx_path), "");
            format!("cd {pdisp} || exit 1\n{setup_prefix}{launch}")
        };

        // 5. Open the pane (split or new window per the existing rule).
        let width = tmux_window_width().unwrap_or(0);
        let out = if width >= 400 {
            std::process::Command::new("tmux")
                .args(["split-window", "-h", "-P", "-F", "#{pane_id}", "-c"])
                .arg(&path)
                .arg(format!("sh -lc {}", shell_escape(&cmd)))
                .output()?
        } else {
            std::process::Command::new("tmux")
                .args(["new-window", "-P", "-F", "#{pane_id}", "-c"])
                .arg(&path)
                .arg(format!("sh -lc {}", shell_escape(&cmd)))
                .output()?
        };
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "tmux pane failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if assistant == "claude" {
            let send_cmd = format!(
                "(sleep 4; tmux send-keys -t {pane} '/remote-control' Enter) >/dev/null 2>&1 &"
            );
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(&send_cmd)
                .spawn();
        }

        let mode = if is_resume { "resumed" } else { "new" };
        self.status = format!("DevQA · {assistant} {mode} @ {} · {pr_url}", path.display());
        Ok(())
    }

    /// Re-open Claude in an already-existing DevQA worktree on disk, without
    /// needing the PR cached. Resumes the saved session if there is one, else
    /// starts a fresh one. Used when the ticket is already past "begin DevQA"
    /// and a worktree is present but the PR/session links are gone.
    async fn reopen_devqa_worktree(&mut self, key: &str, path: std::path::PathBuf) -> Result<()> {
        let assistant = code_assistant_label(&self.code_assistant);
        if std::env::var("TMUX").is_err() {
            self.status = format!(
                "DevQA worktree at {} — open tmux to launch {assistant}",
                path.display()
            );
            return Ok(());
        }
        let (session_id, is_resume) = if assistant_uses_jui_session(assistant) {
            self.get_or_create_assistant_session(key, assistant).await?
        } else {
            (String::new(), false)
        };
        let session_arg = code_assistant_session_arg(assistant, &session_id, is_resume);
        let setup_prefix = match worktree_setup_script(&path) {
            Some(s) => format!("echo '── applying worktree setup ──'\n{s}\n"),
            None => String::new(),
        };
        let pdisp = shell_escape(&path.display().to_string());
        let pr_identity = self.current_pr_identity();
        let (repo, pr_url, pr_number) = match &pr_identity {
            Some((repo, url, number)) => (Some(repo.as_str()), Some(url.as_str()), Some(*number)),
            None => (None, None, None),
        };
        let branch = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
        let cmd = if assistant == "claude" && is_resume {
            let launch = code_assistant_launch_cmd(assistant, &session_arg, None, "");
            format!("cd {pdisp} || exit 1\n{setup_prefix}{launch}")
        } else {
            let ctx = self
                .build_devqa_context(key, repo, pr_url, pr_number, branch.as_deref())
                .await;
            let ctx_path = std::env::temp_dir().join(format!("jui-devqa-{key}.md"));
            std::fs::write(&ctx_path, ctx)?;
            let launch = code_assistant_launch_cmd(assistant, &session_arg, Some(&ctx_path), "");
            format!("cd {pdisp} || exit 1\n{setup_prefix}{launch}")
        };
        let width = tmux_window_width().unwrap_or(0);
        let out = if width >= 400 {
            std::process::Command::new("tmux")
                .args(["split-window", "-h", "-P", "-F", "#{pane_id}", "-c"])
                .arg(&path)
                .arg(format!("sh -lc {}", shell_escape(&cmd)))
                .output()?
        } else {
            std::process::Command::new("tmux")
                .args(["new-window", "-P", "-F", "#{pane_id}", "-c"])
                .arg(&path)
                .arg(format!("sh -lc {}", shell_escape(&cmd)))
                .output()?
        };
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "tmux pane failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if assistant == "claude" {
            let send_cmd = format!(
                "(sleep 4; tmux send-keys -t {pane} '/remote-control' Enter) >/dev/null 2>&1 &"
            );
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(&send_cmd)
                .spawn();
        }
        let mode = if is_resume { "resumed" } else { "new" };
        self.status = format!(
            "DevQA · {assistant} {mode} @ {} (existing worktree)",
            path.display()
        );
        Ok(())
    }

    /// Submit the DevQA prompt: tear it down and run the checkout + Claude launch
    /// with the chosen worktree/branch-in-repo mode. On error, keep the prompt
    /// open and show the message so the user can adjust (e.g. clean a dirty tree).
    pub async fn submit_devqa_prompt(&mut self) -> Result<()> {
        let Mode::DevQaPrompt(form) = &self.mode else {
            return Ok(());
        };
        let key = form.ticket_key.clone();
        let use_worktree = form.use_worktree;
        self.mode = Mode::Detail;
        if let Err(e) = self.begin_devqa_worktree(&key, use_worktree).await {
            self.mode = Mode::DevQaPrompt(DevQaPromptForm {
                ticket_key: key,
                pr_url: self
                    .pr_comments
                    .first()
                    .map(|c| c.pr_url.clone())
                    .unwrap_or_default(),
                use_worktree,
                error: Some(format!("{e:#}")),
            });
        }
        Ok(())
    }

    /// Confirmed DevQA resolve: post "DevQA: Passed" + 🚀 to the PR (daemon),
    /// then best-effort transition the ticket to a "Dev QA Complete"/"Passed"
    /// status. On the GitHub step failing, keep the confirm open with the error.
    pub async fn submit_devqa_resolve(&mut self) -> Result<()> {
        let Mode::DevQaResolveConfirm(form) = &self.mode else {
            return Ok(());
        };
        let key = form.ticket_key.clone();
        let pr_url = form.pr_url.clone();

        // 1. GitHub side (comment + reaction) via the daemon.
        let mut s = ipc::connect().await?;
        match ipc::send_request(
            &mut s,
            &Request::ResolveDevQaPr {
                ticket_key: key.clone(),
            },
        )
        .await?
        {
            Response::Ok => {}
            Response::Err { message } => {
                self.mode = Mode::DevQaResolveConfirm(DevQaResolveForm {
                    ticket_key: key,
                    pr_url,
                    error: Some(message),
                });
                return Ok(());
            }
            other => {
                self.mode = Mode::DevQaResolveConfirm(DevQaResolveForm {
                    ticket_key: key,
                    pr_url,
                    error: Some(format!("unexpected response: {other:?}")),
                });
                return Ok(());
            }
        }
        self.mode = Mode::Detail;
        let mut parts = vec!["DevQA: Passed posted · 🚀".to_string()];

        // 2. Jira: best-effort transition forward. Match the destination status
        //    on common "DevQA complete/passed" spellings; the transition itself
        //    may just be named "Next", so prefer matching the target status.
        let mut s = ipc::connect().await?;
        if let Response::Transitions { items } =
            ipc::send_request(&mut s, &Request::ListTransitions { key: key.clone() }).await?
        {
            let needles = [
                "dev qa complete",
                "dev qa passed",
                "qa complete",
                "qa passed",
                "qa done",
            ];
            let target = items.iter().find(|tr| {
                let dest = tr.to_status.as_deref().unwrap_or("").to_ascii_lowercase();
                let name = tr.name.to_ascii_lowercase();
                needles.iter().any(|n| dest.contains(n) || name.contains(n))
            });
            if let Some(tr) = target {
                let mut s = ipc::connect().await?;
                let transitioned = matches!(
                    ipc::send_request(
                        &mut s,
                        &Request::Transition {
                            key: key.clone(),
                            to: tr.name.clone(),
                        },
                    )
                    .await,
                    Ok(Response::Ok)
                );
                parts.push(format!(
                    "→ {}",
                    tr.to_status.clone().unwrap_or_else(|| tr.name.clone())
                ));
                if transitioned {
                    self.status = format!("{key} · {}", parts.join(" · "));
                    self.refresh_list_preserving_status().await?;
                }
            } else {
                parts.push("no DevQA-complete transition available".into());
            }
        }
        let _ = self.set_pr_state(&key, PrUserState::Completed).await;

        // 3. Remove the DevQA worktree if one was created (no-op for
        //    branch-in-repo). If it has uncommitted changes, don't discard them
        //    silently — pop a confirm prompt instead. Never block resolve.
        let mut s = ipc::connect().await?;
        match ipc::send_request(
            &mut s,
            &Request::CleanupDevQaWorktree {
                ticket_key: key.clone(),
                force: false,
            },
        )
        .await
        {
            Ok(Response::DevQaCleanup {
                removed,
                dirty,
                message,
            }) => {
                if dirty && !removed {
                    // Surface the resolve outcome now, then ask before discarding.
                    self.status = format!("{key} · {}", parts.join(" · "));
                    self.load_detail().await?;
                    self.mode = Mode::DevQaCleanupConfirm(DevQaCleanupForm {
                        ticket_key: key,
                        detail: message,
                    });
                    return Ok(());
                }
                if removed {
                    parts.push(message);
                }
            }
            Ok(Response::Err { message }) => parts.push(format!("worktree cleanup: {message}")),
            _ => {}
        }

        self.status = format!("{key} · {}", parts.join(" · "));
        self.load_detail().await?;
        self.refresh_list_preserving_status().await?;
        Ok(())
    }

    /// Confirmed worktree cleanup from `DevQaCleanupConfirm` — force-remove the
    /// dirty DevQA worktree. (Esc on the prompt keeps it; see the key handler.)
    pub async fn confirm_devqa_cleanup(&mut self) -> Result<()> {
        let Mode::DevQaCleanupConfirm(form) = &self.mode else {
            return Ok(());
        };
        let key = form.ticket_key.clone();
        self.mode = Mode::Detail;
        let mut s = ipc::connect().await?;
        match ipc::send_request(
            &mut s,
            &Request::CleanupDevQaWorktree {
                ticket_key: key.clone(),
                force: true,
            },
        )
        .await
        {
            Ok(Response::DevQaCleanup { message, .. }) => {
                self.status = format!("{key} · {message}")
            }
            Ok(Response::Err { message }) => {
                self.status = format!("{key} · worktree cleanup: {message}")
            }
            _ => {}
        }
        self.load_detail().await?;
        Ok(())
    }

    pub async fn launch_claude_in_tmux(&mut self) -> Result<()> {
        let Mode::Implementation(form) = &self.mode else {
            return Ok(());
        };
        if std::env::var("TMUX").is_err() {
            self.status = "must be running inside tmux to launch a new window".into();
            return Ok(());
        }
        let Some(top) = form.project_paths.iter().find(|p| p.exists()).cloned() else {
            self.status = "no available project path to cd into".into();
            return Ok(());
        };
        let Some(t) = self.detail.clone() else {
            return Ok(());
        };
        let key = t.key.clone();
        let project_paths = form.project_paths.clone();
        let markdown = form.markdown.clone();

        let assistant = code_assistant_label(&self.code_assistant);

        // Reuse an existing session id for assistants with explicit resume
        // support.
        let (session_id, is_resume) = if assistant_uses_jui_session(assistant) {
            self.get_or_create_assistant_session(&key, assistant)
                .await?
        } else {
            (String::new(), false)
        };
        let session_arg = code_assistant_session_arg(assistant, &session_id, is_resume);

        // On resume, prior context is already in the session. On new sessions,
        // pass the context file as the first message.
        let cmd = if assistant_uses_jui_session(assistant) && is_resume {
            code_assistant_launch_cmd(assistant, &session_arg, None, "")
        } else {
            let tmp_dir = std::env::temp_dir();
            let ctx_path = tmp_dir.join(format!("jui-ctx-{}.md", key));
            let context = build_claude_context(&t, &project_paths, &markdown);
            std::fs::write(&ctx_path, context)?;
            code_assistant_launch_cmd(assistant, &session_arg, Some(&ctx_path), "")
        };

        let status = std::process::Command::new("tmux")
            .arg("new-window")
            .arg("-c")
            .arg(&top)
            .arg(format!("sh -lc {}", shell_escape(&cmd)))
            .status()?;
        if !status.success() {
            self.status = "tmux new-window failed".into();
            return Ok(());
        }
        let mode = if is_resume { "resumed" } else { "new" };
        self.status = if assistant_uses_jui_session(assistant) {
            format!(
                "{assistant} {mode} @ {} · session {}",
                top.display(),
                &session_id[..8.min(session_id.len())]
            )
        } else {
            format!("{assistant} {mode} @ {}", top.display())
        };
        if let Mode::Implementation(form) = &mut self.mode {
            form.status_line = format!("{assistant} {mode} in new tmux window");
        }
        Ok(())
    }

    pub async fn open_priority_picker(&mut self) -> Result<()> {
        let Some(t) = &self.detail else { return Ok(()) };
        let key = t.key.clone();
        let current = t.priority.clone().unwrap_or_default();
        self.mode = Mode::EditPriority(EditPriorityForm {
            key,
            options: vec![],
            selected: 0,
            loading: true,
            error: None,
        });
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(&mut s, &Request::ListPriorities).await?;
        if let Mode::EditPriority(form) = &mut self.mode {
            form.loading = false;
            match resp {
                Response::Priorities { items } => {
                    form.options = items;
                    form.selected = form
                        .options
                        .iter()
                        .position(|p| p.eq_ignore_ascii_case(&current))
                        .unwrap_or(0);
                    if form.options.is_empty() {
                        form.error = Some("no priorities available".into());
                    }
                }
                Response::Err { message } => form.error = Some(message),
                _ => form.error = Some("unexpected response".into()),
            }
        }
        Ok(())
    }

    pub async fn submit_priority(&mut self) -> Result<()> {
        let Mode::EditPriority(form) = &self.mode else {
            return Ok(());
        };
        let Some(name) = form.options.get(form.selected) else {
            return Ok(());
        };
        let key = form.key.clone();
        let priority = name.clone();
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::EditPriority {
                key: key.clone(),
                priority: priority.clone(),
            },
        )
        .await?;
        match resp {
            Response::Ok => {
                self.status = format!("{key} priority → {priority}");
                self.mode = Mode::Detail;
                self.load_detail().await?;
            }
            Response::Err { message } => self.status = format!("err: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        Ok(())
    }

    pub async fn open_ticket_projects(&mut self) -> Result<()> {
        let Some(t) = &self.detail else { return Ok(()) };
        let key = t.key.clone();
        self.mode = Mode::TicketProjects(TicketProjectsForm {
            ticket_key: key.clone(),
            items: vec![],
            selected: 0,
            error: None,
        });
        let mut s = ipc::connect().await?;
        let resp =
            ipc::send_request(&mut s, &Request::ListTicketProjects { ticket_key: key }).await?;
        if let Mode::TicketProjects(form) = &mut self.mode {
            match resp {
                Response::TicketProjects { items } => {
                    form.items = items;
                    if form.items.is_empty() {
                        form.error = Some(
                            "no projects configured · open projects (p) and add some first".into(),
                        );
                    }
                }
                Response::Err { message } => form.error = Some(message),
                _ => form.error = Some("unexpected response".into()),
            }
        }
        Ok(())
    }

    pub async fn toggle_ticket_project(&mut self) -> Result<()> {
        let Mode::TicketProjects(form) = &self.mode else {
            return Ok(());
        };
        let Some(item) = form.items.get(form.selected) else {
            return Ok(());
        };
        if item.state == "worktree" {
            self.status =
                "worktree row is detected from git; link the base repo separately if needed".into();
            return Ok(());
        }
        let ticket_key = form.ticket_key.clone();
        let path = item.project.path.clone();
        let req = if item.linked {
            Request::UnlinkProject {
                ticket_key: ticket_key.clone(),
                project_path: path.clone(),
            }
        } else {
            Request::LinkProject {
                ticket_key: ticket_key.clone(),
                project_path: path.clone(),
            }
        };
        let was_linked = item.linked;
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &req).await? {
            Response::Ok => {
                if let Mode::TicketProjects(form) = &mut self.mode {
                    if let Some(item) = form.items.get_mut(form.selected) {
                        item.linked = !was_linked;
                    }
                }
                self.status = if was_linked {
                    format!("unlinked {}", path.display())
                } else {
                    format!("linked {}", path.display())
                };
            }
            Response::Err { message } => self.status = format!("err: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        Ok(())
    }

    pub async fn open_assign_picker(&mut self, purpose: AssignPurpose) -> Result<()> {
        let Some(t) = self.detail.as_ref() else {
            return Ok(());
        };
        self.mode = Mode::AssignPicker(AssignPickerForm {
            key: t.key.clone(),
            purpose,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            error: None,
        });
        Ok(())
    }

    /// Re-run user search and store the new results in the AssignPicker form.
    pub async fn refresh_assign_picker(&mut self) -> Result<()> {
        let q = if let Mode::AssignPicker(f) = &self.mode {
            f.query.clone()
        } else {
            return Ok(());
        };
        let mut s = ipc::connect().await?;
        if let Ok(Response::Users { items, .. }) =
            ipc::send_request(&mut s, &Request::SearchUsers { query: q }).await
        {
            if let Mode::AssignPicker(f) = &mut self.mode {
                f.results = items
                    .into_iter()
                    .map(|u| (u.display_name, u.account_id))
                    .collect();
                if f.selected >= f.results.len() {
                    f.selected = 0;
                }
            }
        }
        Ok(())
    }

    pub async fn submit_assign_picker(&mut self) -> Result<()> {
        let Mode::AssignPicker(form) = &self.mode else {
            return Ok(());
        };
        let key = form.key.clone();
        let purpose = form.purpose;
        let picked: Option<(String, String)> = form.results.get(form.selected).cloned();
        let query_blank = form.query.trim().is_empty();

        // Resolve target user
        let (id, display): (String, String) = match (purpose, picked, query_blank) {
            (AssignPurpose::Assignee, Some((name, id)), _) => (id, name),
            (AssignPurpose::Reviewer, Some((name, id)), _) => (id, name),
            (AssignPurpose::DevQa, Some((name, id)), _) => (id, name),
            (AssignPurpose::Assignee, None, true) => match self.my_account_id.clone() {
                Some(id) => (id, "(me)".into()),
                None => {
                    if let Mode::AssignPicker(f) = &mut self.mode {
                        f.error = Some("no my_account_id loaded — type a name".into());
                    }
                    return Ok(());
                }
            },
            (AssignPurpose::Reviewer, None, _) => {
                if let Mode::AssignPicker(f) = &mut self.mode {
                    f.error = Some("pick a user — reviewer has no default".into());
                }
                return Ok(());
            }
            (AssignPurpose::DevQa, None, _) => {
                if let Mode::AssignPicker(f) = &mut self.mode {
                    f.error = Some("pick a user - DevQA has no default".into());
                }
                return Ok(());
            }
            (AssignPurpose::Assignee, None, false) => {
                if let Mode::AssignPicker(f) = &mut self.mode {
                    f.error = Some("no matches — refine the query or pick from the list".into());
                }
                return Ok(());
            }
        };

        let req = match purpose {
            AssignPurpose::Assignee => Request::AssignTicket {
                key: key.clone(),
                assignee: id,
            },
            AssignPurpose::Reviewer => Request::SetReviewer {
                key: key.clone(),
                assignee_id: id,
            },
            AssignPurpose::DevQa => Request::SetDevQa {
                key: key.clone(),
                assignee_id: id,
            },
        };
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &req).await? {
            Response::Ok => {
                self.status = match purpose {
                    AssignPurpose::Assignee => format!("assigned {key} to {display}"),
                    AssignPurpose::Reviewer => format!("reviewer set on {key}: {display}"),
                    AssignPurpose::DevQa => format!("DevQA set on {key}: {display}"),
                };
                self.mode = Mode::Detail;
                self.load_detail().await?;
            }
            Response::Err { message } => {
                if let Mode::AssignPicker(f) = &mut self.mode {
                    f.error = Some(message);
                }
            }
            _ => {
                if let Mode::AssignPicker(f) = &mut self.mode {
                    f.error = Some("unexpected response".into());
                }
            }
        }
        Ok(())
    }

    pub async fn open_pr_create(&mut self) -> Result<()> {
        let Some(t) = self.detail.as_ref() else {
            return Ok(());
        };
        let key = t.key.clone();
        let title = format!("{}: {}", t.key, t.summary);
        let body = String::from("## Summary\n\n- \n\n## Test plan\n\n- [ ] \n");
        let route_hint = self.pr_route_hint(&key, None).await.unwrap_or_default();
        // Look for a persisted draft so a /review that was interrupted by a
        // restart / crash / closed tmux pane is recoverable.
        let draft = {
            let mut s = ipc::connect().await?;
            match ipc::send_request(
                &mut s,
                &Request::GetPrDraft {
                    ticket_key: key.clone(),
                },
            )
            .await?
            {
                Response::PrDraft { draft } => draft,
                _ => None,
            }
        };
        let title_cursor = title.len();
        let body_cursor = body.len();
        let mut form = PrCreateForm {
            key: key.clone(),
            title,
            body,
            reviewer_query: String::new(),
            reviewer_results: Vec::new(),
            reviewer: None,
            reviewer_picker_selected: 0,
            devqa_query: String::new(),
            devqa_results: Vec::new(),
            devqa: None,
            devqa_picker_selected: 0,
            field: 0,
            busy: false,
            error: None,
            pending_handle: None,
            review_state: PrReviewState::Pending,
            review_output: None,
            review_scroll: 0,
            title_cursor,
            body_cursor,
            suggestion: None,
            remote_pick: None,
            route_hint,
        };
        let mut restored_state: Option<&'static str> = None;
        if let Some(d) = draft {
            form.title = d.title;
            form.body = d.body;
            form.title_cursor = form.title.len();
            form.body_cursor = form.body.len();
            if let (Some(id), Some(name)) = (d.reviewer_account_id, d.reviewer_display_name) {
                form.reviewer = Some((name, id));
            }
            if let (Some(id), Some(name)) = (d.devqa_account_id, d.devqa_display_name) {
                form.devqa = Some((name, id));
            }
            form.review_output = d.review_output;
            form.review_state = if d.review_state.eq_ignore_ascii_case("reviewing") {
                restored_state = Some("reviewing");
                PrReviewState::Reviewing
            } else {
                restored_state = Some("pending");
                PrReviewState::Pending
            };
        }
        self.mode = Mode::PrCreate(form);
        if let Some(s) = restored_state {
            self.status = if s == "reviewing" {
                "resumed PR draft · review previously started · y submit · f fix · R re-run review"
                    .into()
            } else {
                "resumed PR draft · ^S to run /review".into()
            };
        }
        Ok(())
    }

    async fn pr_route_hint(&self, ticket_key: &str, push_remote: Option<&str>) -> Result<String> {
        let path = {
            let mut s = ipc::connect().await?;
            match ipc::send_request(
                &mut s,
                &Request::GetTicketWorktree {
                    ticket_key: ticket_key.to_string(),
                },
            )
            .await?
            {
                Response::TicketWorktree { path } => path,
                _ => None,
            }
        };
        let branch = path
            .as_ref()
            .and_then(|p| {
                std::process::Command::new("git")
                    .args(["-C", p.to_str()?, "branch", "--show-current"])
                    .output()
                    .ok()
                    .filter(|out| out.status.success())
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "<branch>".into());
        let target_repo = path
            .as_ref()
            .and_then(|p| {
                std::process::Command::new("gh")
                    .args([
                        "repo",
                        "view",
                        "--json",
                        "nameWithOwner",
                        "-q",
                        ".nameWithOwner",
                    ])
                    .current_dir(p)
                    .output()
                    .ok()
                    .filter(|out| out.status.success())
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "<target repo>".into());
        let remote = match push_remote {
            Some(r) => Some(r.to_string()),
            None => {
                let mut s = ipc::connect().await?;
                match ipc::send_request(
                    &mut s,
                    &Request::GetPushRemote {
                        ticket_key: ticket_key.to_string(),
                    },
                )
                .await?
                {
                    Response::PushRemote { name } => name,
                    _ => None,
                }
            }
        }
        .unwrap_or_else(|| "<pick remote>".into());
        Ok(format!(
            "push {remote}/{branch} -> PR {target_repo}:develop"
        ))
    }

    /// Persist the in-flight PR draft so a restart or dropped tmux pane
    /// doesn't lose the user's review-gated submit.
    pub async fn save_pr_draft(&mut self) -> Result<()> {
        let Mode::PrCreate(f) = &self.mode else {
            return Ok(());
        };
        let draft = jui_core::cache::PrDraft {
            title: f.title.clone(),
            body: f.body.clone(),
            reviewer_account_id: f.reviewer.as_ref().map(|(_, id)| id.clone()),
            reviewer_display_name: f.reviewer.as_ref().map(|(name, _)| name.clone()),
            devqa_account_id: f.devqa.as_ref().map(|(_, id)| id.clone()),
            devqa_display_name: f.devqa.as_ref().map(|(name, _)| name.clone()),
            review_state: match f.review_state {
                PrReviewState::Pending => "pending".into(),
                PrReviewState::Reviewing => "reviewing".into(),
            },
            review_output: f.review_output.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        let key = f.key.clone();
        let mut s = ipc::connect().await?;
        let _ = ipc::send_request(
            &mut s,
            &Request::SavePrDraft {
                ticket_key: key,
                draft,
            },
        )
        .await?;
        Ok(())
    }

    /// Clear the persisted draft after a successful PR submit.
    pub async fn delete_pr_draft(&mut self, ticket_key: &str) -> Result<()> {
        let mut s = ipc::connect().await?;
        let _ = ipc::send_request(
            &mut s,
            &Request::DeletePrDraft {
                ticket_key: ticket_key.to_string(),
            },
        )
        .await?;
        Ok(())
    }

    pub async fn refresh_pr_picker(&mut self, target_reviewer: bool) -> Result<()> {
        let q = if let Mode::PrCreate(f) = &self.mode {
            if target_reviewer {
                f.reviewer_query.clone()
            } else {
                f.devqa_query.clone()
            }
        } else {
            return Ok(());
        };
        let mut s = ipc::connect().await?;
        if let Ok(Response::Users { items, .. }) =
            ipc::send_request(&mut s, &Request::SearchUsers { query: q }).await
        {
            if let Mode::PrCreate(f) = &mut self.mode {
                let results: Vec<(String, String)> = items
                    .into_iter()
                    .map(|u| (u.display_name, u.account_id))
                    .collect();
                if target_reviewer {
                    f.reviewer_results = results;
                    f.reviewer_picker_selected = 0;
                } else {
                    f.devqa_results = results;
                    f.devqa_picker_selected = 0;
                }
            }
        }
        Ok(())
    }

    /// Submit-key handler for the PR-create modal. The pre-submit review
    /// gate is currently shelved — submit goes straight through. Toggle back
    /// on by restoring the `review_state`-aware match below.
    pub async fn pr_submit_pressed(&mut self) -> Result<()> {
        self.submit_pr_create().await
    }

    /// Fire a headless `claude -p /review` against the ticket's worktree.
    /// The result lands in `PrCreateForm.review_output` and the modal pane
    /// renders it inline — no tmux pane swap required. Re-callable from `R`
    /// in `Reviewing` to refresh the review.
    pub async fn run_pr_review(&mut self) -> Result<()> {
        let key = match &self.mode {
            Mode::PrCreate(f) => f.key.clone(),
            _ => return Ok(()),
        };
        if self.pending_pr_review.is_some() {
            self.status = "review already running — wait…".into();
            return Ok(());
        }
        // Flip the form into `Reviewing` immediately so the modal can render
        // the "running…" placeholder. Clear any previous output + scroll.
        if let Mode::PrCreate(f) = &mut self.mode {
            f.review_state = PrReviewState::Reviewing;
            f.review_output = None;
            f.review_scroll = 0;
            f.error = None;
        }
        // Persist what we have so a restart mid-run lands back in Reviewing
        // (with no output yet) instead of losing the form.
        let _ = self.save_pr_draft().await;
        // Fire-and-forget: main_loop drains the receiver. The /review call
        // is long-running (the daemon waits on `claude -p`), so the TUI
        // must stay responsive while it runs.
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_pr_review = Some(rx);
        self.status = "claude /review running… (esc to cancel after it returns)".into();
        let ticket_key = key.clone();
        tokio::spawn(async move {
            let result: anyhow::Result<String> = async {
                let mut s = ipc::connect().await?;
                let req = Request::CodeReview { ticket_key };
                match ipc::send_request(&mut s, &req).await? {
                    Response::ReviewOutput { markdown } => Ok(markdown),
                    Response::Err { message } => Err(anyhow::anyhow!("{message}")),
                    _ => Err(anyhow::anyhow!("unexpected response")),
                }
            }
            .await;
            let _ = tx.send(result);
        });
        Ok(())
    }

    /// Non-blocking drain for `pending_pr_review`. Called from `main_loop`
    /// each tick. Stores the markdown on the form and persists the draft so
    /// a restart can still see it. Returns `true` when something changed.
    pub fn poll_pending_pr_review(&mut self) -> bool {
        use tokio::sync::oneshot::error::TryRecvError;
        let Some(rx) = self.pending_pr_review.as_mut() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(md)) => {
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.review_output = Some(md);
                    f.review_scroll = 0;
                }
                self.pending_pr_review = None;
                self.status =
                    "review ready · y submit · f fix session · R re-run · esc back".into();
                // Best-effort save with the new output. Spawn so we don't
                // block the UI thread on an IPC round-trip.
                let snapshot = if let Mode::PrCreate(f) = &self.mode {
                    Some((f.key.clone(), self.snapshot_pr_draft()))
                } else {
                    None
                };
                if let Some((key, draft)) = snapshot {
                    tokio::spawn(async move {
                        if let Ok(mut s) = ipc::connect().await {
                            let _ = ipc::send_request(
                                &mut s,
                                &Request::SavePrDraft {
                                    ticket_key: key,
                                    draft,
                                },
                            )
                            .await;
                        }
                    });
                }
                true
            }
            Ok(Err(e)) => {
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.error = Some(format!("{e:#}"));
                }
                self.pending_pr_review = None;
                self.status = format!("review failed: {e:#}");
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Closed) => {
                self.pending_pr_review = None;
                self.status = "review task dropped".into();
                true
            }
        }
    }

    /// Snapshot the current PrCreate form as a PrDraft. Caller must have
    /// already verified `self.mode` is `PrCreate`.
    fn snapshot_pr_draft(&self) -> jui_core::cache::PrDraft {
        let f = match &self.mode {
            Mode::PrCreate(f) => f,
            _ => unreachable!("snapshot_pr_draft called outside PrCreate"),
        };
        jui_core::cache::PrDraft {
            title: f.title.clone(),
            body: f.body.clone(),
            reviewer_account_id: f.reviewer.as_ref().map(|(_, id)| id.clone()),
            reviewer_display_name: f.reviewer.as_ref().map(|(name, _)| name.clone()),
            devqa_account_id: f.devqa.as_ref().map(|(_, id)| id.clone()),
            devqa_display_name: f.devqa.as_ref().map(|(name, _)| name.clone()),
            review_state: match f.review_state {
                PrReviewState::Pending => "pending".into(),
                PrReviewState::Reviewing => "reviewing".into(),
            },
            review_output: f.review_output.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Open a follow-up assistant pane on the same ticket (no `/review`), so the
    /// user can resolve issues the review surfaced. Stays in `Reviewing` so a
    /// subsequent `y` still submits the PR.
    pub async fn open_pr_fix_session(&mut self) -> Result<()> {
        let key = match &self.mode {
            Mode::PrCreate(f) => f.key.clone(),
            _ => return Ok(()),
        };
        let worktree = match self.ticket_worktree(&key).await? {
            Some(p) => p,
            None => {
                self.status = "no worktree on file".into();
                return Ok(());
            }
        };
        self.spawn_claude_pane(&key, &worktree, None).await?;
        self.status = "fix-session pane opened · resolve, then y to submit".into();
        Ok(())
    }

    /// Resolve which git remote to push to for this ticket.
    /// `Use(name)` → caller passes it through. `PickerOpened` → caller must
    /// return; picker handles the next y-press. `NoneAvailable` → no remotes
    /// found at all, let the daemon error out on its own.
    pub async fn resolve_push_remote(&mut self, ticket_key: &str) -> Result<PushRemoteOutcome> {
        // 1. Cached value wins.
        let cached = {
            let mut s = ipc::connect().await?;
            match ipc::send_request(
                &mut s,
                &Request::GetPushRemote {
                    ticket_key: ticket_key.to_string(),
                },
            )
            .await?
            {
                Response::PushRemote { name } => name,
                _ => None,
            }
        };
        if let Some(name) = cached {
            let hint = self
                .pr_route_hint(ticket_key, Some(&name))
                .await
                .unwrap_or_default();
            if let Mode::PrCreate(f) = &mut self.mode {
                f.route_hint = hint;
            }
            return Ok(PushRemoteOutcome::Use(name));
        }
        // 2. No cached value — list remotes.
        let remotes: Vec<(String, String)> = {
            let mut s = ipc::connect().await?;
            match ipc::send_request(
                &mut s,
                &Request::ListWorktreeRemotes {
                    ticket_key: ticket_key.to_string(),
                },
            )
            .await?
            {
                Response::Remotes { items } => items,
                Response::Err { message } => {
                    if let Mode::PrCreate(f) = &mut self.mode {
                        f.error = Some(format!("remote list failed: {message}"));
                    }
                    return Ok(PushRemoteOutcome::NoneAvailable);
                }
                _ => Vec::new(),
            }
        };
        match remotes.len() {
            0 => Ok(PushRemoteOutcome::NoneAvailable),
            1 => {
                // Single remote — auto-pick + persist so we don't ask again.
                let name = remotes[0].0.clone();
                let mut s = ipc::connect().await?;
                let _ = ipc::send_request(
                    &mut s,
                    &Request::SetPushRemote {
                        ticket_key: ticket_key.to_string(),
                        remote_name: name.clone(),
                    },
                )
                .await?;
                let hint = self
                    .pr_route_hint(ticket_key, Some(&name))
                    .await
                    .unwrap_or_default();
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.route_hint = hint;
                }
                Ok(PushRemoteOutcome::Use(name))
            }
            _ => {
                // Multiple — pop the picker. Default selection prefers a
                // non-"origin" remote (assumption: user's fork). Falls back to
                // index 0 if every remote is named "origin" somehow.
                let default = remotes.iter().position(|(n, _)| n != "origin").unwrap_or(0);
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.remote_pick = Some(RemotePickerForm {
                        items: remotes,
                        selected: default,
                    });
                    f.error = None;
                }
                self.status =
                    "pick push remote · j/k: move · enter: save+push · esc: cancel".into();
                Ok(PushRemoteOutcome::PickerOpened)
            }
        }
    }

    /// Commit the remote-picker selection: persist it for this project, close
    /// the overlay, and re-fire submit so the PR goes through with the chosen
    /// remote already cached.
    pub async fn commit_remote_pick(&mut self) -> Result<()> {
        let (ticket_key, remote_name) = {
            let Mode::PrCreate(f) = &self.mode else {
                return Ok(());
            };
            let Some(picker) = f.remote_pick.as_ref() else {
                return Ok(());
            };
            let Some((name, _)) = picker.items.get(picker.selected) else {
                return Ok(());
            };
            (f.key.clone(), name.clone())
        };
        let mut s = ipc::connect().await?;
        let _ = ipc::send_request(
            &mut s,
            &Request::SetPushRemote {
                ticket_key: ticket_key.clone(),
                remote_name: remote_name.clone(),
            },
        )
        .await?;
        if let Mode::PrCreate(f) = &mut self.mode {
            f.remote_pick = None;
        }
        let hint = self
            .pr_route_hint(&ticket_key, Some(&remote_name))
            .await
            .unwrap_or_default();
        if let Mode::PrCreate(f) = &mut self.mode {
            f.route_hint = hint;
        }
        self.status = format!("push remote saved: {remote_name}");
        // Re-fire submit — cached value picks up the new choice.
        self.submit_pr_create().await
    }

    pub async fn submit_pr_create(&mut self) -> Result<()> {
        // Snapshot all the form data we need so we can hold a `&mut self`
        // borrow during the async call without colliding with reads of
        // `self.mode`.
        let snapshot = if let Mode::PrCreate(form) = &self.mode {
            Some((
                form.key.clone(),
                form.title.clone(),
                form.body.clone(),
                form.reviewer.as_ref().map(|(_, id)| id.clone()),
                form.devqa.as_ref().map(|(_, id)| id.clone()),
            ))
        } else {
            None
        };
        let Some((key, title, body, reviewer_id, devqa_id)) = snapshot else {
            return Ok(());
        };
        if title.trim().is_empty() {
            if let Mode::PrCreate(f) = &mut self.mode {
                f.error = Some("title is required".into());
            }
            return Ok(());
        }
        // Resolve push remote before the daemon round-trip. Cached → use it.
        // Otherwise list remotes: 1 = auto-pick; >1 = open the picker and bail
        // (user re-presses y after picking).
        let push_remote = match self.resolve_push_remote(&key).await? {
            PushRemoteOutcome::Use(name) => Some(name),
            PushRemoteOutcome::PickerOpened => return Ok(()),
            PushRemoteOutcome::NoneAvailable => None, // daemon will fall back to "origin"
        };
        if let Mode::PrCreate(f) = &mut self.mode {
            f.busy = true;
            f.error = None;
        }
        let req = Request::CreatePullRequest {
            ticket_key: key.clone(),
            title,
            body,
            reviewer_account_id: reviewer_id,
            devqa_account_id: devqa_id,
            push_remote,
        };
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(&mut s, &req).await;
        match resp {
            Ok(Response::PullRequestCreated { url, number }) => {
                // PR is live — the draft is no longer in flight.
                let _ = self.delete_pr_draft(&key).await;
                self.status = format!("PR #{number} opened: {url}");
                self.mode = Mode::Detail;
                self.load_detail().await?;
                self.refresh_list_preserving_status().await?;
            }
            Ok(Response::Err { message }) => {
                // Detect "no GitHub handle mapped" and offer to set it inline.
                let needs_handle = message.contains("no GitHub handle mapped");
                if needs_handle {
                    // Re-borrow the form to pick the right user.
                    let target = if let Mode::PrCreate(f) = &self.mode {
                        if message.contains("reviewer") {
                            f.reviewer.clone()
                        } else if message.contains("DevQA") {
                            f.devqa.clone()
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some((display, id)) = target {
                        if let Mode::PrCreate(f) = &mut self.mode {
                            f.pending_handle = Some(PendingHandle {
                                account_id: id,
                                display_name: display,
                                handle: String::new(),
                            });
                            f.busy = false;
                            f.error = None;
                            return Ok(());
                        }
                    }
                }
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.error = Some(message);
                    f.busy = false;
                }
            }
            Ok(_) => {
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.error = Some("unexpected daemon response".into());
                    f.busy = false;
                }
            }
            Err(e) => {
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.error = Some(format!("{e:#}"));
                    f.busy = false;
                }
            }
        }
        Ok(())
    }

    /// Persist the just-typed GitHub handle for the pending Jira user, then
    /// retry the PR submission.
    pub async fn submit_pending_handle(&mut self) -> Result<()> {
        let pending = if let Mode::PrCreate(f) = &self.mode {
            f.pending_handle.clone()
        } else {
            return Ok(());
        };
        let Some(p) = pending else { return Ok(()) };
        if p.handle.trim().is_empty() {
            if let Mode::PrCreate(f) = &mut self.mode {
                f.error = Some("enter a GitHub handle (no leading @)".into());
            }
            return Ok(());
        }
        let mut s = ipc::connect().await?;
        match ipc::send_request(
            &mut s,
            &Request::SetGithubHandle {
                account_id: p.account_id.clone(),
                handle: p.handle.clone(),
            },
        )
        .await?
        {
            Response::Ok => {
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.pending_handle = None;
                }
                self.submit_pr_create().await?;
            }
            Response::Err { message } => {
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.error = Some(format!("set handle: {message}"));
                }
            }
            _ => {
                if let Mode::PrCreate(f) = &mut self.mode {
                    f.error = Some("unexpected response".into());
                }
            }
        }
        Ok(())
    }

    /// Build a tree of the user's tickets walking up parent_key chains. The daemon
    /// serves the full set (mine + ancestors) from its SQLite cache in a single IPC
    /// call — the daemon's hourly warmup task keeps the cache populated.
    pub async fn open_tree(&mut self) -> Result<()> {
        use std::collections::HashMap;
        use std::collections::HashSet;

        // Build the role map first so we can tag leaves once we have the data.
        // Assigned wins over Reviewer wins over Mentioned (duplicates dropped).
        let assigned: HashSet<String> = self.tickets.iter().map(|t| t.key.clone()).collect();
        // Drop completed PRs entirely when the K-toggle says so. Otherwise
        // keep them — the children sort below sinks them to the bottom.
        let drop_completed = !self.show_completed_prs;
        let is_completed = |k: &str| -> bool {
            self.pr_user_states.get(k).copied() == Some(PrUserState::Completed)
        };
        let reviewer: HashSet<String> = self
            .reviewing_tickets
            .iter()
            .map(|t| t.key.clone())
            .filter(|k| !assigned.contains(k))
            .filter(|k| !drop_completed || !is_completed(k))
            .collect();
        let github: HashSet<String> = self
            .github_tickets
            .iter()
            .map(|t| t.key.clone())
            .filter(|k| !assigned.contains(k) && !reviewer.contains(k))
            .filter(|k| !drop_completed || !is_completed(k))
            .collect();
        let mentioned: HashSet<String> = self
            .mentioned_tickets
            .iter()
            .map(|t| t.key.clone())
            .filter(|k| !assigned.contains(k) && !reviewer.contains(k) && !github.contains(k))
            .collect();
        let role_for = |k: &str| -> Option<MentionRole> {
            if assigned.contains(k) {
                Some(MentionRole::Assigned)
            } else if reviewer.contains(k) {
                Some(MentionRole::Reviewer)
            } else if github.contains(k) {
                Some(MentionRole::Github)
            } else if mentioned.contains(k) {
                Some(MentionRole::Mentioned)
            } else {
                None
            }
        };
        // Union seed: every ticket from any of the four sources.
        let mut seed: Vec<String> = Vec::new();
        seed.extend(assigned.iter().cloned());
        seed.extend(reviewer.iter().cloned());
        seed.extend(github.iter().cloned());
        seed.extend(mentioned.iter().cloned());

        self.status = "loading tree…".into();

        let mut s = match ipc::connect().await {
            Ok(s) => s,
            Err(e) => {
                self.status = format!("tree: daemon unavailable: {e}");
                return Ok(());
            }
        };
        let resp = ipc::send_request(
            &mut s,
            &Request::GetTicketsWithAncestors { keys: seed.clone() },
        )
        .await?;
        let items: Vec<jui_core::ticket::Ticket> = match resp {
            Response::Tickets { items } => items,
            Response::Err { message } => {
                self.status = format!("tree err: {message}");
                return Ok(());
            }
            _ => {
                self.status = "tree: unexpected response".into();
                return Ok(());
            }
        };

        let mut by_key: HashMap<String, TreeNode> = HashMap::new();
        for t in items {
            let key = t.key.clone();
            let next_pk = match t.parent_key.clone() {
                Some(np) if np != key => Some(np),
                _ => None,
            };
            let role = role_for(&key);
            let is_mine = role.is_some();
            let has_my_open_pr = self.my_authored_pr_keys.contains(&key);
            by_key.insert(
                key,
                TreeNode {
                    key: t.key,
                    summary: t.summary,
                    status: t.status,
                    issue_type: t.issue_type,
                    parent_key: next_pk,
                    children: Vec::new(),
                    depth: 0,
                    expanded: true,
                    is_mine,
                    role,
                    has_my_open_pr,
                },
            );
        }
        // Placeholder nodes for any seed key the cache didn't return — without
        // these, a leaf whose parent_key points to an un-cached ticket would
        // silently lose context.
        for k in &seed {
            let role = role_for(k);
            let has_my_open_pr = self.my_authored_pr_keys.contains(k);
            by_key.entry(k.clone()).or_insert(TreeNode {
                key: k.clone(),
                summary: String::new(),
                status: String::new(),
                issue_type: None,
                parent_key: None,
                children: Vec::new(),
                depth: 0,
                expanded: true,
                is_mine: role.is_some(),
                role,
                has_my_open_pr,
            });
        }

        // Materialize a stable ordering: indices in by_key insertion order won't be
        // deterministic, so collect-and-sort by key.
        let mut keys: Vec<String> = by_key.keys().cloned().collect();
        keys.sort();
        let key_to_idx: HashMap<String, usize> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), i))
            .collect();
        let mut nodes: Vec<TreeNode> = keys.iter().map(|k| by_key.remove(k).unwrap()).collect();

        // Wire parent → children, identify roots. Skip self-parent (would loop).
        let mut roots: Vec<usize> = Vec::new();
        for i in 0..nodes.len() {
            if let Some(pk) = nodes[i].parent_key.clone() {
                if let Some(&pi) = key_to_idx.get(&pk) {
                    if pi != i {
                        nodes[pi].children.push(i);
                        continue;
                    }
                }
            }
            roots.push(i);
        }

        // Sort roots: Epics first, then assigned-without-PR before
        // assigned-with-PR (so "ready to pick up" surfaces above tickets you
        // already have work-in-flight on), then by key.
        roots.sort_by(|&a, &b| {
            let aep = is_epic(&nodes[a]);
            let bep = is_epic(&nodes[b]);
            let pa = nodes[a].has_my_open_pr as u8;
            let pb = nodes[b].has_my_open_pr as u8;
            bep.cmp(&aep)
                .then_with(|| pa.cmp(&pb))
                .then_with(|| nodes[a].key.cmp(&nodes[b].key))
        });
        // Sort each node's children, in priority order:
        //   1. Completed PRs sink to the very bottom.
        //   2. GitHub-mention rows sink under non-GitHub siblings (your own
        //      sub-tasks under a Story show first; PRs you're reviewing
        //      from someone else's branch on the same Story trail behind).
        //   3. Tickets with an open PR you authored sink under tickets without
        //      one — surfaces "ready to pick up" siblings first.
        //   4. Issue-type weight (Story < Task < Sub-task).
        //   5. Ticket key (stable visual order).
        let states_snap = self.pr_user_states.clone();
        let is_done =
            |k: &str| -> bool { states_snap.get(k).copied() == Some(PrUserState::Completed) };
        for i in 0..nodes.len() {
            let mut ch = std::mem::take(&mut nodes[i].children);
            ch.sort_by(|&a, &b| {
                let da = is_done(&nodes[a].key) as u8;
                let db = is_done(&nodes[b].key) as u8;
                let ga = matches!(nodes[a].role, Some(MentionRole::Github)) as u8;
                let gb = matches!(nodes[b].role, Some(MentionRole::Github)) as u8;
                let pa = nodes[a].has_my_open_pr as u8;
                let pb = nodes[b].has_my_open_pr as u8;
                da.cmp(&db)
                    .then_with(|| ga.cmp(&gb))
                    .then_with(|| pa.cmp(&pb))
                    .then_with(|| type_weight(&nodes[a]).cmp(&type_weight(&nodes[b])))
                    .then_with(|| nodes[a].key.cmp(&nodes[b].key))
            });
            nodes[i].children = ch;
        }
        // Compute depth via BFS from roots.
        for &r in &roots {
            walk_depth(&mut nodes, r, 0);
        }

        let mut form = TreeForm {
            nodes,
            roots,
            visible: Vec::new(),
            selected: 0,
            two_column: false,
        };
        recompute_tree_visible(&mut form);
        self.mode = Mode::Tree(form);
        self.status = "tree".into();
        Ok(())
    }

    /// Open the workflow-status editor. Lists `active_statuses` from the
    /// in-memory app state (loaded from config at startup).
    pub fn open_active_status_config(&mut self) {
        self.mode = Mode::ActiveStatusConfig(ActiveStatusForm {
            items: self.active_statuses.clone(),
            selected: 0,
            pending_remove: None,
            adding: None,
        });
        self.status = "workflow statuses — i: add · d×2: delete · esc: close".into();
    }

    /// Mark the currently-selected PR review thread as resolved on GitHub.
    /// Issue-thread / review-wrapper comments don't belong to a thread, so
    /// we surface an error in the status bar without round-tripping.
    pub async fn resolve_selected_pr_comment(&mut self) -> Result<()> {
        let Some(ticket) = self.detail.as_ref().map(|t| t.key.clone()) else {
            self.status = "no ticket open".into();
            return Ok(());
        };
        let visible = self.visible_pr_comments();
        let Some(&real_idx) = visible.get(self.pr_comment_selected) else {
            self.status = "no PR comment selected".into();
            return Ok(());
        };
        let Some(c) = self.pr_comments.get(real_idx).cloned() else {
            self.status = "no PR comment selected".into();
            return Ok(());
        };
        if !c.kind.eq_ignore_ascii_case("review") {
            self.status = format!(
                "can't resolve a `{}` comment — only inline review threads are resolvable",
                c.kind
            );
            return Ok(());
        }
        if c.comment_id.is_empty() {
            self.status =
                "comment id missing (cached before migration) — wait for next refresh and retry"
                    .into();
            return Ok(());
        }
        self.status = "resolving thread…".into();
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::ResolvePrComment {
                ticket_key: ticket.clone(),
                comment_id: c.comment_id.clone(),
            },
        )
        .await;
        match resp {
            Ok(Response::Ok) => {
                self.status = format!("resolved thread for comment {}", c.comment_id);
                // Optimistically mark locally so the hide-resolved filter
                // hides it on next draw, before the async daemon refresh
                // lands. Any comment in the same thread shares the state —
                // safest approximation is to flip just this row; the next
                // refresh fills in any siblings.
                if let Some(local) = self.pr_comments.get_mut(real_idx) {
                    local.is_resolved = true;
                }
                // Best-effort re-fetch so siblings + new replies show up.
                let mut s = ipc::connect().await?;
                if let Ok(Response::PrComments { items, pr_link }) =
                    ipc::send_request(&mut s, &Request::ListPrComments { ticket_key: ticket }).await
                {
                    self.pr_comments = items;
                    self.detail_pr_link = pr_link;
                    let v = self.visible_pr_comments();
                    if !v.is_empty() {
                        self.pr_comment_selected = self.pr_comment_selected.min(v.len() - 1);
                    } else {
                        self.pr_comment_selected = 0;
                    }
                }
            }
            Ok(Response::Err { message }) => {
                self.status = format!("resolve failed: {message}");
            }
            Ok(_) => self.status = "unexpected daemon response".into(),
            Err(e) => self.status = format!("resolve err: {e:#}"),
        }
        Ok(())
    }

    /// Open the reply modal for the currently-selected PR comment. Threaded
    /// review replies route to GitHub's `/pulls/{n}/comments/{id}/replies`
    /// endpoint; issue-thread and review_wrapper kinds drop a new top-level
    /// comment on the PR.
    pub fn open_pr_comment_reply(&mut self) {
        let Some(ticket) = self.detail.as_ref().map(|t| t.key.clone()) else {
            self.status = "no ticket open".into();
            return;
        };
        let visible = self.visible_pr_comments();
        let Some(&real_idx) = visible.get(self.pr_comment_selected) else {
            self.status = "no PR comment selected".into();
            return;
        };
        let Some(c) = self.pr_comments.get(real_idx).cloned() else {
            self.status = "no PR comment selected".into();
            return;
        };
        self.mode = Mode::PrCommentReply(PrCommentReplyForm {
            ticket_key: ticket,
            parent_kind: c.kind,
            parent_id: c.comment_id,
            parent_author: c.author,
            parent_body: c.body,
            body: String::new(),
            body_cursor: 0,
            busy: false,
            error: None,
        });
        self.status = "type reply · ^S / F5 submit · esc cancel".into();
    }

    pub async fn submit_pr_comment_reply(&mut self) -> Result<()> {
        let snapshot = if let Mode::PrCommentReply(f) = &self.mode {
            if f.body.trim().is_empty() {
                None
            } else {
                Some((
                    f.ticket_key.clone(),
                    f.parent_kind.clone(),
                    f.parent_id.clone(),
                    f.body.clone(),
                ))
            }
        } else {
            None
        };
        let Some((ticket_key, parent_kind, parent_id, body)) = snapshot else {
            if let Mode::PrCommentReply(f) = &mut self.mode {
                f.error = Some("reply body is empty".into());
            }
            return Ok(());
        };
        if let Mode::PrCommentReply(f) = &mut self.mode {
            f.busy = true;
            f.error = None;
        }
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::ReplyToPrComment {
                ticket_key: ticket_key.clone(),
                parent_kind,
                parent_id,
                body,
            },
        )
        .await;
        match resp {
            Ok(Response::Ok) => {
                self.status = "reply posted".into();
                self.mode = Mode::Detail;
                // Best-effort: re-pull the merged comment list so the new
                // row shows up without waiting for the next warmup tick.
                let mut s = ipc::connect().await?;
                if let Ok(Response::PrComments { items, pr_link }) =
                    ipc::send_request(&mut s, &Request::ListPrComments { ticket_key }).await
                {
                    self.pr_comments = items;
                    self.detail_pr_link = pr_link;
                    let v = self.visible_pr_comments();
                    if !v.is_empty() {
                        self.pr_comment_selected = self.pr_comment_selected.min(v.len() - 1);
                    } else {
                        self.pr_comment_selected = 0;
                    }
                }
            }
            Ok(Response::Err { message }) => {
                if let Mode::PrCommentReply(f) = &mut self.mode {
                    f.error = Some(message);
                    f.busy = false;
                }
            }
            Ok(_) => {
                if let Mode::PrCommentReply(f) = &mut self.mode {
                    f.error = Some("unexpected daemon response".into());
                    f.busy = false;
                }
            }
            Err(e) => {
                if let Mode::PrCommentReply(f) = &mut self.mode {
                    f.error = Some(format!("{e:#}"));
                    f.busy = false;
                }
            }
        }
        Ok(())
    }

    /// Open an assistant session about the currently-selected PR comment. Resumes
    /// the ticket's stored session id (or starts fresh) and seeds the pane
    /// with `Copilot suggested this in code review: "<body>". What are your
    /// thoughts?` so the conversation begins in context.
    pub async fn chat_about_pr_comment(&mut self) -> Result<()> {
        let Some(ticket) = self.detail.as_ref().map(|t| t.key.clone()) else {
            self.status = "no ticket open".into();
            return Ok(());
        };
        let visible = self.visible_pr_comments();
        let Some(&real_idx) = visible.get(self.pr_comment_selected) else {
            self.status = "no PR comment selected".into();
            return Ok(());
        };
        let Some(comment) = self.pr_comments.get(real_idx).cloned() else {
            self.status = "no PR comment selected".into();
            return Ok(());
        };
        let worktree = match self.ticket_worktree(&ticket).await? {
            Some(p) => p,
            None => {
                self.status =
                    "no worktree on file — start work (s) or link a project (P) first".into();
                return Ok(());
            }
        };
        let author = if comment.author.is_empty() {
            "Reviewer".to_string()
        } else {
            comment.author.clone()
        };
        let prompt = format!(
            "{author} suggested this in code review:\n\n\"\"\"\n{body}\n\"\"\"\n\nWhat are your thoughts?",
            body = comment.body.trim(),
        );
        let assistant = code_assistant_label(&self.code_assistant).to_string();
        self.spawn_claude_pane(&ticket, &worktree, Some(&prompt))
            .await?;
        self.status = format!("{assistant} opened on PR comment by {author}");
        Ok(())
    }

    /// Spawn (or resume) the configured code assistant in a tmux pane rooted at `worktree`,
    /// reusing the ticket's stored session id. When `initial_input` is set,
    /// a detached subshell sleeps 4s and `tmux send-keys` it as the first
    /// line so commands like `/review` or a question fire automatically.
    /// Returns the new pane id (best-effort; empty on failure).
    pub async fn spawn_claude_pane(
        &mut self,
        ticket_key: &str,
        worktree: &std::path::Path,
        initial_input: Option<&str>,
    ) -> Result<String> {
        if std::env::var("TMUX").is_err() {
            let assistant = code_assistant_label(&self.code_assistant);
            self.status = format!(
                "not in tmux — run: cd {} && {}",
                worktree.display(),
                code_assistant_cmd(assistant, None),
            );
            return Ok(String::new());
        }
        let assistant = code_assistant_label(&self.code_assistant);
        // Get or create the session id so future pane spawns share context.
        let (session_id, is_resume) = self
            .get_or_create_assistant_session(ticket_key, assistant)
            .await?;
        let session_arg = code_assistant_session_arg(assistant, &session_id, is_resume);
        let initial_input_path = if assistant == "opencode" {
            if let Some(input) = initial_input {
                let path = std::env::temp_dir().join(format!("jui-opencode-{ticket_key}.md"));
                std::fs::write(&path, input)?;
                Some(path)
            } else {
                None
            }
        } else {
            None
        };
        let launch =
            code_assistant_launch_cmd(assistant, &session_arg, initial_input_path.as_deref(), "");
        let cmd = format!(
            "cd {} && {}",
            shell_escape(&worktree.display().to_string()),
            launch,
        );
        let width = tmux_window_width().unwrap_or(0);
        let pane = if width >= 400 {
            let out = std::process::Command::new("tmux")
                .args(["split-window", "-h", "-P", "-F", "#{pane_id}"])
                .arg(format!("sh -lc {}", shell_escape(&cmd)))
                .output()?;
            if !out.status.success() {
                self.status = format!(
                    "tmux split failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                return Ok(String::new());
            }
            let _ = std::process::Command::new("tmux")
                .args(["select-layout", "even-horizontal"])
                .status();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            let out = std::process::Command::new("tmux")
                .args(["new-window", "-P", "-F", "#{pane_id}"])
                .arg(format!("sh -lc {}", shell_escape(&cmd)))
                .output()?;
            if !out.status.success() {
                self.status = format!(
                    "tmux new-window failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                return Ok(String::new());
            }
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        // Detached send-keys after a settle delay so Claude has the REPL up.
        if assistant != "opencode" {
            if let Some(input) = initial_input {
                let send_cmd = format!(
                    "(sleep 4; tmux send-keys -t {pane} {escaped} Enter) >/dev/null 2>&1 &",
                    pane = pane,
                    escaped = shell_escape(input),
                );
                let _ = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&send_cmd)
                    .spawn();
            }
        }
        Ok(pane)
    }

    /// Ask the daemon for the on-disk worktree path tied to a ticket. Falls
    /// back to `None` when no linked project or the worktree dir is missing.
    pub async fn ticket_worktree(
        &mut self,
        ticket_key: &str,
    ) -> Result<Option<std::path::PathBuf>> {
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::GetTicketWorktree {
                ticket_key: ticket_key.to_string(),
            },
        )
        .await?;
        Ok(match resp {
            Response::TicketWorktree { path } => path,
            _ => None,
        })
    }

    /// Snapshot the current view as a NavFrame and push it onto `nav_stack`
    /// so a subsequent `pop_back_or_quit()` can restore it. Returns the
    /// pushed frame (or `None` if the current mode isn't navigable). Modal
    /// forms (Comment, Create, EditTime, pickers, etc.) are deliberately
    /// not navigable — they're returned-to by Esc.
    pub fn push_current_view(&mut self) -> Option<()> {
        let frame = match &self.mode {
            Mode::List => Some(NavFrame::List),
            Mode::Archive => Some(NavFrame::Archive),
            Mode::Kanban | Mode::KanbanFilter(_) => Some(NavFrame::Kanban),
            Mode::Tree(f) => Some(NavFrame::Tree(Box::new(f.clone()))),
            Mode::Home(_) => Some(NavFrame::Home),
            Mode::Settings(_) => Some(NavFrame::Settings),
            Mode::Rules(_) => Some(NavFrame::Rules),
            Mode::RuleLog(_) => Some(NavFrame::RuleLog),
            Mode::RuleEdit(f) => Some(NavFrame::RuleEdit(
                f.original_id.clone().unwrap_or_default(),
            )),
            Mode::Projects(_) => Some(NavFrame::Projects),
            Mode::PullRequests(_) => Some(NavFrame::PullRequests),
            Mode::ConfluenceSpaces(_) | Mode::ConfluencePages(_) | Mode::PageView(_) => {
                Some(NavFrame::Confluence)
            }
            Mode::ActiveStatusConfig(_) => Some(NavFrame::ActiveStatusConfig),
            Mode::Detail => Some(NavFrame::Detail {
                ticket_key: self
                    .detail
                    .as_ref()
                    .map(|t| t.key.clone())
                    .unwrap_or_default(),
                focus: self.detail_focus,
                subtask_selected: self.subtask_selected,
                comment_selected: self.comment_selected,
            }),
            _ => None,
        };
        let frame = frame?;
        self.nav_stack.push(frame);
        Some(())
    }

    /// Q-back: pop the top of the nav stack and reopen that view. If the
    /// stack is empty, set `should_quit`. Returns `Ok(true)` when a view
    /// was popped, `Ok(false)` when the app is being asked to exit.
    pub async fn pop_back_or_quit(&mut self) -> Result<bool> {
        let Some(frame) = self.nav_stack.pop() else {
            self.should_quit = true;
            return Ok(false);
        };
        match frame {
            NavFrame::List => self.mode = Mode::List,
            NavFrame::Archive => self.mode = Mode::Archive,
            NavFrame::Kanban => self.mode = Mode::Kanban,
            NavFrame::Tree(form) => self.mode = Mode::Tree(*form),
            NavFrame::Detail {
                ticket_key,
                focus,
                subtask_selected,
                comment_selected,
            } => {
                if !ticket_key.is_empty() {
                    self.open_ticket_by_key(ticket_key).await?;
                }
                self.detail_focus = focus;
                self.subtask_selected = subtask_selected;
                self.comment_selected = comment_selected;
                self.mode = Mode::Detail;
            }
            NavFrame::Home => {
                // Reopen Home without re-pushing (we're popping).
                self.mode = Mode::Home(HomeForm {
                    items: Vec::new(),
                    selected: 0,
                    loading: true,
                    error: None,
                    menu_selected: 0,
                    focus: HomeFocus::Menu,
                });
                if let Err(e) = self.load_home_activity().await {
                    self.status = format!("activity err: {e:#}");
                }
            }
            NavFrame::Settings => self.open_settings(),
            NavFrame::Rules => self.open_rules(),
            NavFrame::RuleLog => self.open_rule_log().await?,
            NavFrame::RuleEdit(_) => {
                // We don't capture full editor state; pop to Rules instead so
                // the user can pick the rule again. Practical for v1.
                self.open_rules();
            }
            NavFrame::Projects => self.open_projects().await?,
            NavFrame::PullRequests => self.open_pull_requests().await?,
            NavFrame::Confluence => self.open_confluence_spaces().await?,
            NavFrame::ActiveStatusConfig => self.open_active_status_config(),
        }
        Ok(true)
    }

    /// Return to the Home pane and reload its activity feed. Used as the
    /// "back" target from every top-level view (Settings, Rules, Kanban,
    /// Tree, Archive, Confluence, Projects, ActiveStatusConfig).
    pub async fn go_home(&mut self) -> Result<()> {
        self.mode = Mode::Home(HomeForm {
            items: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
            menu_selected: 0,
            focus: HomeFocus::Menu,
        });
        if let Err(e) = self.load_home_activity().await {
            self.status = format!("activity err: {e:#}");
        }
        Ok(())
    }

    /// Refresh the Home pane's activity feed from the daemon. Idempotent;
    /// safe to call on startup and on user-triggered refresh.
    pub async fn load_home_activity(&mut self) -> Result<()> {
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(&mut s, &Request::RecentActivity { limit: 50 }).await?;
        if let Mode::Home(form) = &mut self.mode {
            form.loading = false;
            match resp {
                Response::Activity { items } => {
                    form.items = items;
                    form.selected = form.selected.min(form.items.len().saturating_sub(1));
                }
                Response::Err { message } => form.error = Some(message),
                _ => form.error = Some("unexpected response".into()),
            }
        }
        Ok(())
    }

    /// Switch from Home to a named target view. Each branch mirrors the
    /// existing keybind that opens that view from List. Pushes Home onto
    /// the back stack so Q from the destination pops here.
    pub async fn home_open(&mut self, target: HomeTarget) -> Result<()> {
        self.push_current_view();
        match target {
            HomeTarget::List => {
                self.mode = Mode::List;
            }
            HomeTarget::Tree => {
                self.open_tree().await?;
            }
            HomeTarget::Kanban => {
                self.mode = Mode::Kanban;
            }
            HomeTarget::Archive => {
                self.mode = Mode::Archive;
            }
            HomeTarget::PullRequests => {
                self.open_pull_requests().await?;
            }
            HomeTarget::Confluence => {
                self.open_confluence_spaces().await?;
            }
            HomeTarget::Settings => {
                self.open_settings();
            }
            HomeTarget::Rules => {
                self.open_rules();
            }
            HomeTarget::Projects => {
                self.open_projects().await?;
            }
        }
        Ok(())
    }

    /// Open the rules-engine fire-history pane. Fetches the most recent
    /// 500 log entries (well within the 5-day rolling window for any
    /// reasonable cadence) from the daemon.
    pub async fn open_rule_log(&mut self) -> Result<()> {
        self.mode = Mode::RuleLog(RuleLogForm {
            items: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
        });
        self.status = "loading rule log…".into();
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(&mut s, &Request::ListRuleLog { limit: 500 }).await?;
        if let Mode::RuleLog(form) = &mut self.mode {
            form.loading = false;
            match resp {
                Response::RuleLog { items } => {
                    form.items = items;
                    self.status = format!(
                        "rule log — {} entries (5-day rolling) · j/k scroll · r refresh · esc back",
                        form.items.len()
                    );
                }
                Response::Err { message } => {
                    form.error = Some(message.clone());
                    self.status = format!("rule log err: {message}");
                }
                _ => {
                    form.error = Some("unexpected response".into());
                    self.status = "unexpected response".into();
                }
            }
        }
        Ok(())
    }

    /// Open the rules engine list pane. Rules are loaded fresh from
    /// `~/.config/jui/config.toml` so external edits show up.
    pub fn open_rules(&mut self) {
        let cfg = jui_core::config::GlobalConfig::load().unwrap_or_default();
        self.mode = Mode::Rules(RulesForm {
            items: cfg.rules,
            selected: 0,
            pending_remove: None,
        });
        self.status = "rules — a add · d delete · t toggle · enter edit · esc back".into();
    }

    /// Save the current rules list back to `~/.config/jui/config.toml`.
    /// Called after every add/delete/toggle so the user doesn't have to
    /// remember a save key, mirroring the active-statuses editor.
    pub fn save_rules(&mut self) -> Result<()> {
        let Mode::Rules(form) = &self.mode else {
            return Ok(());
        };
        let items = form.items.clone();
        let mut cfg = jui_core::config::GlobalConfig::load().unwrap_or_default();
        cfg.rules = items;
        cfg.save()?;
        Ok(())
    }

    /// Persist a single rule edit back into the rules list, then return to
    /// the list view. If `original_id` is `None`, the rule is pushed as new;
    /// otherwise the existing rule with that id is replaced in place.
    pub fn save_rule_edit(&mut self) -> Result<()> {
        let (working, original) = match &self.mode {
            Mode::RuleEdit(f) => (f.rule.clone(), f.original_id.clone()),
            _ => return Ok(()),
        };
        let mut cfg = jui_core::config::GlobalConfig::load().unwrap_or_default();
        match original {
            Some(id) => {
                if let Some(slot) = cfg.rules.iter_mut().find(|r| r.id == id) {
                    *slot = working;
                } else {
                    cfg.rules.push(working);
                }
            }
            None => cfg.rules.push(working),
        }
        cfg.save()?;
        self.open_rules();
        self.status = "rule saved".into();
        Ok(())
    }

    /// Open the general Settings page. Values are seeded from the cached
    /// fields on `App`; commits write through `save_settings`.
    pub fn open_settings(&mut self) {
        self.mode = Mode::Settings(SettingsForm {
            selected: 0,
            default_create_status: self.default_create_status.clone(),
            all_mine_exclude_status: self.all_mine_exclude_status.clone(),
            pr_submit_status: self.pr_submit_status.clone(),
            code_assistant: self.code_assistant.clone(),
            claude_permission_mode: self.claude_permission_mode.clone(),
            picker: None,
        });
        self.status = "settings — i/enter: pick status · j/k: move · esc: close".into();
    }

    /// Open the status-picker overlay for the active settings row and kick off
    /// a daemon round-trip to fetch every Jira status. The current value is
    /// pre-selected once results arrive.
    pub async fn open_settings_picker(&mut self) -> Result<()> {
        let (row, current) = if let Mode::Settings(form) = &self.mode {
            let cur = match form.selected {
                0 => form.default_create_status.clone(),
                1 => form.all_mine_exclude_status.clone(),
                2 => form.pr_submit_status.clone(),
                3 => form.code_assistant.clone(),
                4 => form.claude_permission_mode.clone(),
                _ => String::new(),
            };
            (form.selected, cur)
        } else {
            return Ok(());
        };
        // Rows 3–4 pick from fixed lists — no daemon
        // round-trip, the options are baked into the binary.
        if row == 3 || row == 4 {
            if let Mode::Settings(form) = &mut self.mode {
                let source = if row == 3 {
                    jui_core::config::CODE_ASSISTANTS
                } else {
                    jui_core::config::CLAUDE_PERMISSION_MODES
                };
                let all: Vec<String> = source.iter().map(|s| s.to_string()).collect();
                let selected = all
                    .iter()
                    .position(|m| m.eq_ignore_ascii_case(&current))
                    .unwrap_or(0);
                form.picker = Some(StatusPicker {
                    row,
                    query: String::new(),
                    all,
                    selected,
                    loading: false,
                    error: None,
                });
            }
            self.status = "type: filter · j/k: move · enter: pick · esc: cancel".into();
            return Ok(());
        }
        if let Mode::Settings(form) = &mut self.mode {
            form.picker = Some(StatusPicker {
                row,
                query: String::new(),
                all: Vec::new(),
                selected: 0,
                loading: true,
                error: None,
            });
        }
        self.status = "fetching statuses…".into();
        let resp = {
            let mut s = ipc::connect().await?;
            ipc::send_request(&mut s, &Request::ListStatuses).await?
        };
        if let Mode::Settings(form) = &mut self.mode {
            let Some(p) = form.picker.as_mut() else {
                return Ok(());
            };
            p.loading = false;
            match resp {
                Response::Statuses { items } => {
                    if let Some(pos) = items.iter().position(|s| s.eq_ignore_ascii_case(&current)) {
                        p.selected = pos;
                    }
                    p.all = items;
                    self.status = "type: filter · j/k: move · enter: pick · esc: cancel".into();
                }
                Response::Err { message } => {
                    p.error = Some(message.clone());
                    self.status = format!("status fetch failed: {message}");
                }
                _ => {
                    p.error = Some("unexpected response".into());
                    self.status = "unexpected response".into();
                }
            }
        }
        Ok(())
    }

    /// Persist the current settings form back to `GlobalConfig.workflow` and
    /// refresh the cached fields on `App`.
    pub fn save_settings(&mut self) -> Result<()> {
        let (create, exclude, pr_submit, assistant, perm_mode) =
            if let Mode::Settings(form) = &self.mode {
                (
                    form.default_create_status.clone(),
                    form.all_mine_exclude_status.clone(),
                    form.pr_submit_status.clone(),
                    form.code_assistant.clone(),
                    form.claude_permission_mode.clone(),
                )
            } else {
                return Ok(());
            };
        let mut cfg = jui_core::config::GlobalConfig::load().unwrap_or_default();
        cfg.workflow.default_create_status = create.clone();
        cfg.workflow.all_mine_exclude_status = exclude.clone();
        cfg.workflow.pr_submit_status = pr_submit.clone();
        cfg.workflow.code_assistant = code_assistant_label(&assistant).to_string();
        cfg.workflow.claude_permission_mode = perm_mode.clone();
        cfg.save()?;
        self.default_create_status = create;
        self.all_mine_exclude_status = exclude;
        self.pr_submit_status = pr_submit;
        self.code_assistant = code_assistant_label(&assistant).to_string();
        self.claude_permission_mode = perm_mode;
        Ok(())
    }

    /// Persist the current form's items back to `GlobalConfig.workflow` and
    /// refresh the app's in-memory copy. Called after every add/delete so the
    /// user doesn't have to remember a "save" key.
    pub fn save_active_statuses(&mut self) -> Result<()> {
        let Mode::ActiveStatusConfig(form) = &self.mode else {
            return Ok(());
        };
        let items = form.items.clone();
        let mut cfg = jui_core::config::GlobalConfig::load().unwrap_or_default();
        cfg.workflow.active_statuses = items.clone();
        cfg.save()?;
        self.active_statuses = items;
        Ok(())
    }

    pub async fn open_projects(&mut self) -> Result<()> {
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(&mut s, &Request::ListProjects).await?;
        let items = match resp {
            Response::Projects { items } => items,
            Response::Err { message } => {
                self.status = format!("err: {message}");
                vec![]
            }
            _ => vec![],
        };
        self.mode = Mode::Projects(ProjectsForm {
            items,
            selected: 0,
            pending_remove: None,
        });
        Ok(())
    }

    pub async fn open_pull_requests(&mut self) -> Result<()> {
        self.mode = Mode::PullRequests(PullRequestsForm {
            items: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
        });
        self.refresh_pull_requests().await
    }

    pub async fn refresh_pull_requests(&mut self) -> Result<()> {
        let resp = match ipc::connect().await {
            Ok(mut s) => ipc::send_request(&mut s, &Request::ListMyPullRequests).await,
            Err(e) => Err(e),
        };
        if let Mode::PullRequests(form) = &mut self.mode {
            form.loading = false;
            match resp {
                Ok(Response::PullRequests { items }) => {
                    form.items = items;
                    form.selected = form.selected.min(form.items.len().saturating_sub(1));
                    self.status = format!("{} open PRs", form.items.len());
                }
                Ok(Response::Err { message }) => form.error = Some(message),
                Ok(_) => form.error = Some("unexpected response".into()),
                Err(e) => {
                    let message = format!(
                        "PR list unavailable: {e:#}. If this started after an update, restart jui-daemon."
                    );
                    form.error = Some(message.clone());
                    self.status = message;
                }
            }
        }
        Ok(())
    }

    pub async fn open_selected_pull_request_ticket(&mut self) -> Result<()> {
        let key = match &self.mode {
            Mode::PullRequests(form) => form
                .items
                .get(form.selected)
                .and_then(|pr| pr.ticket_key.clone()),
            _ => None,
        };
        let Some(key) = key else {
            self.status = "selected PR branch has no Jira ticket key".into();
            return Ok(());
        };
        self.push_current_view();
        self.detail_origin = DetailOrigin::List;
        self.detail_focus = DetailFocus::Info;
        self.open_ticket_by_key(key).await?;
        self.mode = Mode::Detail;
        Ok(())
    }

    pub fn launch_copilot_fix_for_selected_pr_in_app(&mut self) -> Result<()> {
        if self
            .copilot_fix_job
            .as_ref()
            .map(|job| job.is_running())
            .unwrap_or(false)
        {
            self.push_current_view();
            self.mode = Mode::CopilotFixRun(CopilotFixForm {
                scroll_from_bottom: 0,
            });
            self.status = "showing running Copilot fixer".into();
            return Ok(());
        }
        let pr = match &self.mode {
            Mode::PullRequests(form) => form.items.get(form.selected).cloned(),
            _ => None,
        };
        let Some(pr) = pr else { return Ok(()) };
        if !pr.has_unresolved_copilot_comments {
            self.status = "selected PR has no unresolved Copilot comments".into();
            return Ok(());
        }
        let Some(worktree) = pr.worktree_path.clone() else {
            self.status =
                "unresolved Copilot comments found, but no worktree for this branch".into();
            return Ok(());
        };
        let script = PathBuf::from("/mnt/workspace/tools/opencode_pr_comment_autofix.py");
        if !script.exists() {
            self.status = format!("resolve script missing: {}", script.display());
            return Ok(());
        }

        let worktree_arg = shell_escape(&worktree.display().to_string());
        let script_arg = shell_escape(&script.display().to_string());
        let cmd = format!("python3 -u {script_arg} --worktree {worktree_arg}");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut child = tokio::process::Command::new("setsid")
            .arg("script")
            .arg("-q")
            .arg("-e")
            .arg("-f")
            .arg("-c")
            .arg(&cmd)
            .arg("/dev/null")
            .current_dir(&worktree)
            .env("PYTHONUNBUFFERED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(stdout) = child.stdout.take() {
            spawn_copilot_output_reader(stdout, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_copilot_output_reader(stderr, tx.clone());
        }

        let mut output = Vec::new();
        output.push(format!("$ cd {}", worktree.display()));
        output.push(format!("$ {cmd}"));
        output.push(String::new());
        self.copilot_fix_job = Some(CopilotFixJob {
            repo: pr.repo,
            number: pr.number,
            worktree_path: worktree,
            command: cmd,
            output,
            status: CopilotFixStatus::Running,
            child: Some(child),
            rx,
        });
        self.push_current_view();
        self.mode = Mode::CopilotFixRun(CopilotFixForm {
            scroll_from_bottom: 0,
        });
        self.status = "Copilot fixer running — K kills, Esc/q/b keeps running in background".into();
        Ok(())
    }

    pub fn poll_copilot_fix_job(&mut self) {
        let Some(job) = self.copilot_fix_job.as_mut() else {
            return;
        };
        let mut saw_output = false;
        while let Ok(event) = job.rx.try_recv() {
            match event {
                CopilotFixEvent::Line(line) => {
                    job.output.push(line);
                    saw_output = true;
                }
            }
        }
        if job.output.len() > 2_000 {
            let drop = job.output.len() - 2_000;
            job.output.drain(0..drop);
        }
        if !job.is_running() {
            return;
        }
        let Some(child) = job.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code();
                job.status = CopilotFixStatus::Exited(code);
                job.child = None;
                job.output.push(String::new());
                job.output.push(match code {
                    Some(code) => format!("[copilot fixer exited with status {code}]"),
                    None => "[copilot fixer exited by signal]".to_string(),
                });
                self.status = match code {
                    Some(0) => format!("Copilot fixer finished for {}#{}", job.repo, job.number),
                    Some(code) => format!(
                        "Copilot fixer exited with status {code} for {}#{}",
                        job.repo, job.number
                    ),
                    None => format!("Copilot fixer stopped for {}#{}", job.repo, job.number),
                };
            }
            Ok(None) => {
                if saw_output && !matches!(self.mode, Mode::CopilotFixRun(_)) {
                    self.status = format!(
                        "Copilot fixer still running for {}#{}",
                        job.repo, job.number
                    );
                }
            }
            Err(e) => {
                job.status = CopilotFixStatus::Failed(format!("{e:#}"));
                job.child = None;
                job.output.push(format!("[failed to poll child: {e:#}]"));
                self.status = format!("Copilot fixer poll failed: {e:#}");
            }
        }
    }

    pub fn kill_copilot_fix_job(&mut self) {
        let Some(job) = self.copilot_fix_job.as_mut() else {
            self.status = "no Copilot fixer is running".into();
            return;
        };
        if !job.is_running() {
            self.status = "Copilot fixer is not running".into();
            return;
        }
        let Some(child) = job.child.as_mut() else {
            self.status = "Copilot fixer child is gone".into();
            return;
        };
        if let Some(pid) = child.id() {
            let pgid = format!("-{pid}");
            let _ = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(&pgid)
                .status();
        }
        let _ = child.start_kill();
        job.status = CopilotFixStatus::KillRequested;
        job.output.push("[kill requested]".into());
        self.status = "kill requested for Copilot fixer".into();
    }

    pub async fn background_copilot_fix_job(&mut self) -> Result<()> {
        let running = self
            .copilot_fix_job
            .as_ref()
            .map(|job| job.is_running())
            .unwrap_or(false);
        self.pop_back_or_quit().await?;
        self.status = if running {
            "Copilot fixer is still running in the background".into()
        } else {
            "Copilot fixer output closed".into()
        };
        Ok(())
    }

    pub async fn open_projects_add(&mut self) -> Result<()> {
        self.mode = Mode::ProjectsAdd(ProjectsAddForm {
            query: String::new(),
            repos: vec![],
            selected: 0,
            loading: true,
            error: None,
        });
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::ScanRepos {
                root: PathBuf::new(),
                max_depth: 6,
            },
        )
        .await?;
        if let Mode::ProjectsAdd(form) = &mut self.mode {
            form.loading = false;
            match resp {
                Response::Repos { items } => {
                    form.repos = items;
                    form.repos.sort_by(|a, b| a.path.cmp(&b.path));
                    if form.repos.is_empty() {
                        form.error = Some("no git/svn repos found under $HOME".into());
                    }
                }
                Response::Err { message } => form.error = Some(message),
                _ => form.error = Some("unexpected response".into()),
            }
        }
        Ok(())
    }

    pub async fn submit_add_project(&mut self) -> Result<()> {
        let Mode::ProjectsAdd(form) = &self.mode else {
            return Ok(());
        };
        let filtered = form.filtered();
        let Some(idx) = filtered.get(form.selected) else {
            return Ok(());
        };
        let Some(repo) = form.repos.get(*idx) else {
            return Ok(());
        };
        let path = repo.path.clone();
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::AddProject {
                path: path.clone(),
                nickname: None,
            },
        )
        .await?;
        match resp {
            Response::Ok => {
                self.status = format!("added {}", path.display());
                self.open_projects().await?;
            }
            Response::Err { message } => self.status = format!("add failed: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        Ok(())
    }

    pub async fn remove_selected_project(&mut self) -> Result<()> {
        let Mode::Projects(form) = &self.mode else {
            return Ok(());
        };
        let Some(p) = form.items.get(form.selected) else {
            return Ok(());
        };
        let path = p.path.clone();
        let mut s = ipc::connect().await?;
        let resp =
            ipc::send_request(&mut s, &Request::RemoveProject { path: path.clone() }).await?;
        match resp {
            Response::Ok => {
                self.status = format!("removed {}", path.display());
                self.open_projects().await?;
            }
            Response::Err { message } => self.status = format!("remove failed: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        Ok(())
    }

    pub async fn submit_edit_time(&mut self) -> Result<()> {
        let Mode::EditTime(form) = &self.mode else {
            return Ok(());
        };
        let key = form.key.clone();
        let original = trim_to_opt(&form.original_estimate);
        let log = trim_to_opt(&form.log_work);
        if original.is_none() && log.is_none() {
            self.status = "nothing to update".into();
            self.mode = Mode::Detail;
            return Ok(());
        }
        let mut s = ipc::connect().await?;
        if let Some(orig) = &original {
            let req = Request::SetEstimate {
                key: key.clone(),
                original: Some(orig.clone()),
                remaining: None,
            };
            match ipc::send_request(&mut s, &req).await? {
                Response::Ok => {}
                Response::Err { message } => {
                    self.status = format!("err: {message}");
                    return Ok(());
                }
                _ => {
                    self.status = "unexpected response".into();
                    return Ok(());
                }
            }
            s = ipc::connect().await?;
        }
        if let Some(time) = &log {
            let req = Request::LogWork {
                key: key.clone(),
                time_spent: time.clone(),
                comment: None,
                new_estimate: None,
            };
            match ipc::send_request(&mut s, &req).await? {
                Response::Ok => {}
                Response::Err { message } => {
                    self.status = format!("err: {message}");
                    return Ok(());
                }
                _ => {
                    self.status = "unexpected response".into();
                    return Ok(());
                }
            }
        }
        self.status = match (original, log) {
            (Some(_), Some(t)) => format!("updated estimate, logged {t}"),
            (Some(o), None) => format!("estimate set to {o}"),
            (None, Some(t)) => format!("logged {t}"),
            _ => "ok".into(),
        };
        self.mode = Mode::Detail;
        self.load_detail().await?;
        Ok(())
    }

    pub async fn submit_transition(&mut self) -> Result<()> {
        let Mode::Transition(form) = &self.mode else {
            return Ok(());
        };
        let Some(opt) = form.options.get(form.selected) else {
            return Ok(());
        };
        let key = form.key.clone();
        // When multiple transitions land on the same destination status,
        // Jira workflows commonly expose a generic one (literally named
        // "Next") alongside a specific one (e.g. "Start Progress" landing
        // on "In Progress"). The generic transition is often wired up to
        // server-side post-functions / Automation rules (WIP-limit mirrors,
        // assignee bumps, etc.) that the specific one is not. The Jira web
        // UI's status button always fires the *specific* transition; the
        // picker modal exposes both, so a user selecting "Next" silently
        // gets the side-effects.
        //
        // Prefer the specific sibling when the user picks a generic one
        // (currently: "Next") that has a same-destination peer. The peer
        // is chosen by: (1) name == destination status (case-insensitive),
        // (2) name != "Next" otherwise. Logs which substitution was made
        // so /tmp/jui.log shows the rewrite.
        let preferred = pick_preferred_transition(&form.options, opt);
        let chosen = preferred.unwrap_or(opt);
        if !std::ptr::eq(chosen, opt) {
            tracing::info!(
                key = %key,
                from = %opt.name,
                from_dest = ?opt.to_status,
                to = %chosen.name,
                to_dest = ?chosen.to_status,
                "transition picker: rewrote generic 'Next' to specific peer to avoid post-function side-effects"
            );
        }
        // jira-cli's `issue move` matches against the *transition name* (e.g.
        // "Start Code Review"), not the destination status (e.g. "Code Rvw").
        let target = chosen.name.clone();
        let to_status = chosen.to_status.clone();
        let req = Request::Transition {
            key: key.clone(),
            to: target.clone(),
        };
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &req).await? {
            Response::Ok => {
                let label = to_status.unwrap_or_else(|| target.clone());
                self.status = format!("{} → {}", key, label);
                self.mode = Mode::Detail;
                self.load_detail().await?;
                self.refresh_list_preserving_status().await?;
            }
            Response::Err { message } => self.status = format!("err: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        Ok(())
    }

    pub async fn open_confluence_spaces(&mut self) -> Result<()> {
        self.mode = Mode::ConfluenceSpaces(ConfluenceSpacesForm {
            spaces: vec![],
            selected: 0,
            loading: true,
            error: None,
        });
        let result: Result<Vec<_>> =
            match try_ipc_for_confluence(&ipc::Request::ConfluenceListSpaces).await {
                Some(ConfluenceData::Spaces(s)) => Ok(s),
                Some(_) | None => {
                    match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
                        Ok(api) => api.list_spaces().await,
                        Err(e) => Err(e),
                    }
                }
            };
        if let Mode::ConfluenceSpaces(form) = &mut self.mode {
            form.loading = false;
            match result {
                Ok(spaces) => form.spaces = spaces,
                Err(e) => form.error = Some(format!("{e:#}")),
            }
        }
        Ok(())
    }

    pub async fn open_confluence_pages(
        &mut self,
        space_key: String,
        space_name: String,
    ) -> Result<()> {
        self.mode = Mode::ConfluencePages(ConfluencePagesForm {
            space_key: space_key.clone(),
            space_name,
            breadcrumb: vec![],
            pages: vec![],
            selected: 0,
            loading: true,
            error: None,
            search_active: false,
            search_query: String::new(),
            search_results: vec![],
            search_selected: 0,
            search_loading: false,
            search_error: None,
            search_submitted: false,
        });
        let req = ipc::Request::ConfluenceListPages {
            space_key: space_key.clone(),
            parent_id: None,
        };
        let result: Result<Vec<_>> = match try_ipc_for_confluence(&req).await {
            Some(ConfluenceData::Pages(p)) => Ok(p),
            Some(_) | None => match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
                Ok(api) => api.list_pages(&space_key).await,
                Err(e) => Err(e),
            },
        };
        if let Mode::ConfluencePages(form) = &mut self.mode {
            form.loading = false;
            match result {
                Ok(pages) => form.pages = pages,
                Err(e) => form.error = Some(format!("{e:#}")),
            }
        }
        Ok(())
    }

    pub async fn open_page_view(&mut self) -> Result<()> {
        let (page_id, space_key, prev_pages) = {
            let Mode::ConfluencePages(form) = &self.mode else {
                return Ok(());
            };
            let page = if form.search_active {
                form.search_results.get(form.search_selected)
            } else {
                form.pages.get(form.selected)
            };
            let Some(page) = page else { return Ok(()) };
            let prev = Box::new(ConfluencePagesForm {
                space_key: form.space_key.clone(),
                space_name: form.space_name.clone(),
                breadcrumb: form.breadcrumb.clone(),
                pages: form.pages.clone(),
                selected: form.selected,
                loading: false,
                error: None,
                search_active: form.search_active,
                search_query: form.search_query.clone(),
                search_results: form.search_results.clone(),
                search_selected: form.search_selected,
                search_loading: false,
                search_error: None,
                search_submitted: form.search_submitted,
            });
            (page.id.clone(), form.space_key.clone(), prev)
        };
        self.status = "fetching…".into();
        let api = match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
            Ok(a) => a,
            Err(e) => {
                self.status = format!("config error: {e:#}");
                return Ok(());
            }
        };
        let (title, html) = match api.get_page_html(&page_id).await {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("fetch error: {e:#}");
                return Ok(());
            }
        };
        let token = jui_core::confluence_api::ConfluenceApi::api_token().unwrap_or_default();
        let html =
            download_confluence_images(&html, &api.server, &api.login, &token, &page_id).await;
        let markdown = html_to_markdown(&html);
        let term_cols = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(120);
        let img_cols = term_cols.saturating_sub(4).max(40);
        if self.picker.is_none() {
            let mut p = match ratatui_image::picker::Picker::from_query_stdio() {
                Ok(p) => p,
                Err(_) => ratatui_image::picker::Picker::from_fontsize((10, 20)),
            };
            // Allow user override: JUI_IMAGE_PROTOCOL=halfblocks|kitty|sixel|iterm2
            if let Ok(name) = std::env::var("JUI_IMAGE_PROTOCOL") {
                use ratatui_image::picker::ProtocolType;
                let t = match name.to_ascii_lowercase().as_str() {
                    "halfblocks" => ProtocolType::Halfblocks,
                    "kitty" => ProtocolType::Kitty,
                    "sixel" => ProtocolType::Sixel,
                    "iterm2" => ProtocolType::Iterm2,
                    _ => p.protocol_type(),
                };
                p.set_protocol_type(t);
            }
            self.picker = Some(p);
        }
        let picker = self.picker.as_mut().expect("picker initialized above");
        let proto = format!("{:?}", picker.protocol_type());
        let (lines, images) = markdown_to_page_lines(&markdown, img_cols, picker);
        self.status = format!("viewing: {} [proto: {}]", title, proto);
        self.mode = Mode::PageView(PageViewForm {
            page_id,
            space_key,
            title,
            markdown,
            lines,
            images,
            scroll: 0,
            viewport_height: 20,
            search_active: false,
            search_query: String::new(),
            search_matches: vec![],
            search_cursor: 0,
            prev_pages,
        });
        Ok(())
    }

    pub async fn page_view_open_editor(&mut self) -> Result<()> {
        let Mode::PageView(form) = &self.mode else {
            return Ok(());
        };
        if std::env::var("TMUX").is_err() {
            self.status = "not in tmux — start jui inside a tmux session".into();
            return Ok(());
        }
        let path = format!("/tmp/confluence-{}.md", form.page_id);
        let content = format!(
            "<!-- Space: {} -->\n<!-- Title: {} -->\n\n# {}\n\n{}",
            form.space_key, form.title, form.title, form.markdown
        );
        std::fs::write(&path, &content)?;
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        let cmd = format!("{} {}", editor, shell_escape(&path));
        let st = std::process::Command::new("tmux")
            .args([
                "split-window",
                "-h",
                &format!("sh -lc {}", shell_escape(&cmd)),
            ])
            .status()?;
        if st.success() {
            self.status = format!("'{}' open in pane — S to sync back", form.title);
        } else {
            self.status = "tmux split-window failed".into();
        }
        Ok(())
    }

    pub async fn page_view_sync(&mut self) -> Result<()> {
        let (page_id, title) = {
            let Mode::PageView(form) = &self.mode else {
                return Ok(());
            };
            (form.page_id.clone(), form.title.clone())
        };
        let path = format!("/tmp/confluence-{}.md", page_id);
        if !std::path::Path::new(&path).exists() {
            self.status = "no local file — open with e first, then save".into();
            return Ok(());
        }
        let api = match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
            Ok(a) => a,
            Err(e) => {
                self.status = format!("config error: {e:#}");
                return Ok(());
            }
        };
        let token = jui_core::confluence_api::ConfluenceApi::api_token().unwrap_or_default();
        let out = std::process::Command::new("mark")
            .args([
                "-u",
                &api.login,
                "-p",
                &token,
                "-b",
                &api.server,
                "-f",
                &path,
                "--minor-edit",
            ])
            .output()?;
        if out.status.success() {
            self.status = format!("synced '{}' to Confluence", title);
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            self.status = format!("sync failed: {}", err.trim());
        }
        Ok(())
    }

    pub async fn confluence_drill_down(&mut self) -> Result<()> {
        let (page_id, space_key) = {
            let Mode::ConfluencePages(form) = &mut self.mode else {
                return Ok(());
            };
            let Some(page) = form.pages.get(form.selected) else {
                return Ok(());
            };
            let id = page.id.clone();
            let title = page.title.clone();
            form.breadcrumb.push((id.clone(), title));
            form.loading = true;
            form.error = None;
            (id, form.space_key.clone())
        };
        let req = ipc::Request::ConfluenceListPages {
            space_key,
            parent_id: Some(page_id.clone()),
        };
        let result: Result<Vec<_>> = match try_ipc_for_confluence(&req).await {
            Some(ConfluenceData::Pages(p)) => Ok(p),
            Some(_) | None => match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
                Ok(api) => api.get_children(&page_id).await,
                Err(e) => Err(e),
            },
        };
        if let Mode::ConfluencePages(form) = &mut self.mode {
            form.loading = false;
            form.selected = 0;
            match result {
                Ok(pages) => form.pages = pages,
                Err(e) => {
                    form.error = Some(format!("{e:#}"));
                    form.breadcrumb.pop();
                }
            }
        }
        Ok(())
    }

    pub async fn confluence_go_back(&mut self) -> Result<()> {
        let (space_key, parent_id) = {
            let Mode::ConfluencePages(form) = &mut self.mode else {
                return Ok(());
            };
            form.breadcrumb.pop();
            let parent_id = form.breadcrumb.last().map(|(id, _)| id.clone());
            form.loading = true;
            form.error = None;
            (form.space_key.clone(), parent_id)
        };
        let req = ipc::Request::ConfluenceListPages {
            space_key: space_key.clone(),
            parent_id: parent_id.clone(),
        };
        let result: Result<Vec<_>> = match try_ipc_for_confluence(&req).await {
            Some(ConfluenceData::Pages(p)) => Ok(p),
            Some(_) | None => match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
                Ok(api) => match parent_id {
                    Some(ref id) => api.get_children(id).await,
                    None => api.list_pages(&space_key).await,
                },
                Err(e) => Err(e),
            },
        };
        if let Mode::ConfluencePages(form) = &mut self.mode {
            form.loading = false;
            form.selected = 0;
            form.search_active = false;
            form.search_query = String::new();
            form.search_results = vec![];
            form.search_submitted = false;
            match result {
                Ok(pages) => form.pages = pages,
                Err(e) => form.error = Some(format!("{e:#}")),
            }
        }
        Ok(())
    }

    pub async fn confluence_search(&mut self) -> Result<()> {
        let (space_key, ancestor_id, query) = {
            let Mode::ConfluencePages(form) = &mut self.mode else {
                return Ok(());
            };
            if form.search_query.trim().is_empty() {
                return Ok(());
            }
            form.search_loading = true;
            form.search_error = None;
            let ancestor = form.breadcrumb.last().map(|(id, _)| id.clone());
            (form.space_key.clone(), ancestor, form.search_query.clone())
        };
        let api = match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
            Ok(a) => a,
            Err(e) => {
                if let Mode::ConfluencePages(form) = &mut self.mode {
                    form.search_loading = false;
                    form.search_error = Some(format!("{e:#}"));
                }
                return Ok(());
            }
        };
        let result = api
            .search_pages(&space_key, ancestor_id.as_deref(), &query)
            .await;
        if let Mode::ConfluencePages(form) = &mut self.mode {
            form.search_loading = false;
            form.search_selected = 0;
            match result {
                Ok(pages) => {
                    form.search_results = pages;
                    form.search_submitted = true;
                }
                Err(e) => form.search_error = Some(format!("{e:#}")),
            }
        }
        Ok(())
    }

    pub async fn confluence_sync(&mut self) -> Result<()> {
        let page_id = {
            let Mode::ConfluencePages(form) = &self.mode else {
                return Ok(());
            };
            let page = if form.search_active {
                form.search_results.get(form.search_selected)
            } else {
                form.pages.get(form.selected)
            };
            let Some(page) = page else { return Ok(()) };
            page.id.clone()
        };
        let path = format!("/tmp/confluence-{}.md", page_id);
        if !std::path::Path::new(&path).exists() {
            self.status = "no local file — press enter to open the page first".into();
            return Ok(());
        }
        let api = match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
            Ok(a) => a,
            Err(e) => {
                self.status = format!("config error: {e:#}");
                return Ok(());
            }
        };
        let token = std::env::var("CONFLUENCE_API_TOKEN")
            .or_else(|_| std::env::var("JIRA_API_TOKEN"))
            .unwrap_or_default();
        self.status = "syncing…".into();
        let result = tokio::process::Command::new("mark")
            .args([
                "-u",
                &api.login,
                "-p",
                &token,
                "-b",
                &api.server,
                "-f",
                &path,
                "--minor-edit",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        self.status = match result {
            Ok(s) if s.success() => "synced to Confluence".into(),
            Ok(_) => "sync failed — check mark config / credentials".into(),
            Err(_) => "mark not found in PATH — install mark for sync".into(),
        };
        Ok(())
    }
}

// ── Page viewer rendering ────────────────────────────────────────────────────

fn md_flush(result: &mut Vec<PageLine>, current: &mut Vec<ratatui::text::Span<'static>>) {
    if !current.is_empty() {
        result.push(PageLine::Spans(std::mem::take(current)));
    }
}

fn md_style(
    base: ratatui::style::Style,
    bold: bool,
    italic: bool,
    link: bool,
) -> ratatui::style::Style {
    use ratatui::style::{Color, Modifier};
    let mut s = base;
    if bold {
        s = s.add_modifier(Modifier::BOLD);
    }
    if italic {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if link {
        s = s.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
    }
    s
}

/// Send a user-search to the daemon and load the results into the Create form's
/// assignee picker. Errors are swallowed — picker stays as-is on transport problems.
async fn refresh_assignee_picker(app: &mut App, query: &str) -> Result<()> {
    let mut s = ipc::connect().await?;
    if let Ok(Response::Users { items, .. }) = ipc::send_request(
        &mut s,
        &Request::SearchUsers {
            query: query.to_string(),
        },
    )
    .await
    {
        if let Mode::Create(f) = &mut app.mode {
            f.assignee_results = items
                .into_iter()
                .map(|u| (u.display_name, u.account_id))
                .collect();
            if f.assignee_picker_selected >= f.assignee_results.len() {
                f.assignee_picker_selected = 0;
            }
        }
    }
    Ok(())
}

/// "Started" if the ticket is in an active workflow status OR a worktree exists for it
/// at the conventional `<repo>/worktrees/<slug>` path.
/// Best-effort: does the current Detail ticket already have a GitHub PR
/// associated with it? Used to suppress the `P` (open PR) keybind so the user
/// doesn't re-open a PR for a ticket where one already exists.
///
/// Heuristics:
///   1. Cached PR comments are non-empty (daemon's github mentions refresh
///      populates these when the ticket's branch matches a known PR).
pub fn ticket_has_pr(app: &App) -> bool {
    !app.pr_comments.is_empty() || app.detail_pr_link.is_some()
}

/// True when the currently-open ticket's Jira status indicates DevQA has been
/// started (i.e. it's in some "Dev QA In Progress" state). Site workflows prefix
/// the status with a team name (e.g. "Firmware Dev QA In Progress"), so we match
/// on a substring rather than the exact label.
pub fn ticket_devqa_in_progress(app: &App) -> bool {
    app.detail
        .as_ref()
        .map(|t| t.status.to_ascii_lowercase().contains("dev qa in progress"))
        .unwrap_or(false)
}

pub fn detail_ticket_assigned_to_me(app: &App) -> bool {
    let Some(t) = app.detail.as_ref() else {
        return false;
    };
    if let Some(me) = app.my_display_name.as_deref() {
        if t.assignee.as_deref() == Some(me) {
            return true;
        }
    }
    app.tickets.iter().any(|mine| mine.key == t.key)
}

/// When the user selects a transition from the picker modal, prefer a
/// more-specific same-destination sibling over a generic "Next" pick.
///
/// Returns `Some(better)` if a better candidate exists, else `None`
/// (caller should keep the user's original pick).
///
/// Rationale: Jira workflows expose generic transitions (often literally
/// named "Next") that move between adjacent states. They're frequently
/// the carrier for project-level Automation rules / post-functions
/// (WIP-limit mirrors, assignee bumps, ticket re-status side-effects)
/// while the same destination's named transition (e.g. "Start Progress"
/// landing on "In Progress") is the clean one the web UI uses. Picking
/// "Next" from the modal therefore appears to "just work" but silently
/// triggers cross-ticket side-effects.
///
/// Selection rules (only triggers when `opt.name` is the generic "Next"):
///   1. A peer landing on the same `to_status` whose `name` equals its
///      own `to_status` (case-insensitive) — the most "natural" pairing.
///   2. Any peer landing on the same `to_status` whose `name` is not
///      "Next" — at least more specific than the generic.
fn pick_preferred_transition<'a>(
    options: &'a [TransitionOption],
    opt: &'a TransitionOption,
) -> Option<&'a TransitionOption> {
    if !opt.name.eq_ignore_ascii_case("Next") {
        return None;
    }
    let dest = opt.to_status.as_deref()?;
    let same_dest = |tr: &&TransitionOption| -> bool {
        tr.to_status
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case(dest))
            .unwrap_or(false)
            && !std::ptr::eq(*tr, opt)
    };
    if let Some(tr) = options
        .iter()
        .filter(same_dest)
        .find(|tr| tr.name.eq_ignore_ascii_case(dest))
    {
        return Some(tr);
    }
    options
        .iter()
        .filter(same_dest)
        .find(|tr| !tr.name.eq_ignore_ascii_case("Next"))
}

/// True when `t.status` matches one of the user-configured active workflow
/// states (case-insensitive). Configured via `Mode::ActiveStatusConfig` →
/// `GlobalConfig.workflow.active_statuses`.
pub fn ticket_status_active(status: &str, active_statuses: &[String]) -> bool {
    let s = status.to_ascii_lowercase();
    active_statuses
        .iter()
        .any(|cand| cand.to_ascii_lowercase() == s)
}

fn is_epic(n: &TreeNode) -> bool {
    n.issue_type
        .as_deref()
        .map(|t| t.eq_ignore_ascii_case("epic"))
        .unwrap_or(false)
}

/// Lower number = sorted earlier (closer to top of children list).
fn type_weight(n: &TreeNode) -> u8 {
    match n.issue_type.as_deref().unwrap_or("") {
        t if t.eq_ignore_ascii_case("epic") => 0,
        t if t.eq_ignore_ascii_case("story") => 1,
        t if t.eq_ignore_ascii_case("task") => 2,
        t if t.eq_ignore_ascii_case("bug") => 3,
        t if t.eq_ignore_ascii_case("sub-task") || t.eq_ignore_ascii_case("subtask") => 4,
        _ => 5,
    }
}

fn walk_depth(nodes: &mut [TreeNode], root: usize, root_depth: u16) {
    let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut stack: Vec<(usize, u16)> = vec![(root, root_depth)];
    while let Some((idx, depth)) = stack.pop() {
        if !visited.insert(idx) {
            continue;
        }
        nodes[idx].depth = depth;
        for &c in &nodes[idx].children.clone() {
            if !visited.contains(&c) {
                stack.push((c, depth + 1));
            }
        }
    }
}

/// Rebuild the flat `visible` list by walking roots and following expanded children.
pub fn recompute_tree_visible(form: &mut TreeForm) {
    let mut out = Vec::new();
    let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let roots = form.roots.clone();
    for r in roots {
        push_visible(&form.nodes, r, &mut out, &mut visited);
    }
    if form.selected >= out.len() && !out.is_empty() {
        form.selected = out.len() - 1;
    }
    form.visible = out;
}

fn push_visible(
    nodes: &[TreeNode],
    idx: usize,
    out: &mut Vec<usize>,
    visited: &mut std::collections::HashSet<usize>,
) {
    if !visited.insert(idx) {
        return;
    }
    out.push(idx);
    if nodes[idx].expanded {
        for &c in &nodes[idx].children {
            push_visible(nodes, c, out, visited);
        }
    }
}

/// Convert markdown string to display lines + images for the page viewer.
pub fn markdown_to_page_lines(
    markdown: &str,
    max_img_cols: u16,
    picker: &mut ratatui_image::picker::Picker,
) -> (Vec<PageLine>, Vec<PageImage>) {
    use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Span;

    let mut result: Vec<PageLine> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut images: Vec<PageImage> = Vec::new();

    let mut bold = false;
    let mut italic = false;
    let mut in_link = false;
    let mut base_style = Style::default();
    let mut in_code_block = false;
    let mut in_blockquote = false;
    let mut in_image = false;
    let mut image_url = String::new();
    // Stack: None = unordered, Some(counter) = ordered
    let mut list_stack: Vec<Option<u64>> = Vec::new();

    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    for event in Parser::new_ext(markdown, opts) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                md_flush(&mut result, &mut current);
                base_style = match level {
                    HeadingLevel::H1 => Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    HeadingLevel::H2 => Style::default()
                        .fg(Color::LightBlue)
                        .add_modifier(Modifier::BOLD),
                    HeadingLevel::H3 => Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default().add_modifier(Modifier::BOLD),
                };
                let hashes = "#".repeat(level as usize) + " ";
                current.push(Span::styled(hashes, base_style));
            }
            Event::End(TagEnd::Heading(_)) => {
                md_flush(&mut result, &mut current);
                base_style = Style::default();
                result.push(PageLine::Blank);
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                md_flush(&mut result, &mut current);
                if list_stack.is_empty() && !in_blockquote {
                    result.push(PageLine::Blank);
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                md_flush(&mut result, &mut current);
                let lang = match kind {
                    CodeBlockKind::Fenced(l) if !l.is_empty() => format!(" {}", l),
                    _ => String::new(),
                };
                result.push(PageLine::Spans(vec![Span::styled(
                    format!("┄┄┄┄┄{}", lang),
                    Style::default().fg(Color::DarkGray),
                )]));
                in_code_block = true;
                base_style = Style::default().fg(Color::Yellow);
            }
            Event::End(TagEnd::CodeBlock) => {
                md_flush(&mut result, &mut current);
                result.push(PageLine::Spans(vec![Span::styled(
                    "┄┄┄┄┄┄┄┄┄┄".to_string(),
                    Style::default().fg(Color::DarkGray),
                )]));
                in_code_block = false;
                base_style = Style::default();
                result.push(PageLine::Blank);
            }
            Event::Start(Tag::BlockQuote(_)) => {
                in_blockquote = true;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                md_flush(&mut result, &mut current);
                in_blockquote = false;
                result.push(PageLine::Blank);
            }
            Event::Start(Tag::List(start)) => {
                list_stack.push(start.map(|n| n.saturating_sub(1)));
            }
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                if list_stack.is_empty() {
                    result.push(PageLine::Blank);
                }
            }
            Event::Start(Tag::Item) => {
                md_flush(&mut result, &mut current);
                let depth = list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                if in_blockquote {
                    current.push(Span::styled(
                        "│ ".to_string(),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                if let Some(counter) = list_stack.last_mut() {
                    match counter {
                        Some(n) => {
                            *n += 1;
                            let num = *n;
                            current.push(Span::raw(indent));
                            current.push(Span::styled(
                                format!("{}. ", num),
                                Style::default().fg(Color::Yellow),
                            ));
                        }
                        None => {
                            current.push(Span::raw(indent));
                            current.push(Span::styled(
                                "• ".to_string(),
                                Style::default().fg(Color::Yellow),
                            ));
                        }
                    }
                }
            }
            Event::End(TagEnd::Item) => {
                md_flush(&mut result, &mut current);
            }
            Event::Start(Tag::Strong) => {
                bold = true;
            }
            Event::End(TagEnd::Strong) => {
                bold = false;
            }
            Event::Start(Tag::Emphasis) => {
                italic = true;
            }
            Event::End(TagEnd::Emphasis) => {
                italic = false;
            }
            Event::Start(Tag::Link { .. }) => {
                in_link = true;
            }
            Event::End(TagEnd::Link) => {
                in_link = false;
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                in_image = true;
                image_url = dest_url.to_string();
            }
            Event::End(TagEnd::Image) => {
                in_image = false;
                md_flush(&mut result, &mut current);
                let url = std::mem::take(&mut image_url);
                let path = url.strip_prefix("file://").unwrap_or(&url);
                let decoded = image::ImageReader::open(path)
                    .ok()
                    .and_then(|r| r.with_guessed_format().ok())
                    .and_then(|r| r.decode().ok());
                if let Some(img) = decoded {
                    let (orig_w, orig_h) = (img.width().max(1), img.height().max(1));
                    let (cell_w, cell_h) = picker.font_size();
                    let max_w_px = (max_img_cols as u32) * (cell_w as u32);
                    let target_w_px = max_w_px.min(orig_w);
                    let target_h_px = orig_h * target_w_px / orig_w;
                    let h_cells = ((target_h_px as f32) / (cell_h as f32)).ceil() as u16;
                    let height = h_cells.max(1);
                    let proto = picker.new_resize_protocol(img);
                    let id = images.len();
                    images.push(PageImage { proto });
                    for r in 0..height {
                        result.push(PageLine::Image { id, row: r, height });
                    }
                } else {
                    result.push(PageLine::Spans(vec![
                        Span::styled("[img] ".to_string(), Style::default().fg(Color::DarkGray)),
                        Span::styled(url, Style::default().fg(Color::Blue)),
                    ]));
                }
                result.push(PageLine::Blank);
            }
            Event::Code(text) => {
                current.push(Span::styled(
                    format!("`{}`", text),
                    Style::default().fg(Color::Yellow),
                ));
            }
            Event::Text(text) => {
                if in_image {
                    continue;
                }
                if in_code_block {
                    for (i, line) in text.split('\n').enumerate() {
                        if i > 0 {
                            md_flush(&mut result, &mut current);
                        }
                        current.push(Span::styled("  ".to_string(), Style::default()));
                        current.push(Span::styled(line.to_string(), base_style));
                    }
                } else {
                    if in_blockquote && current.is_empty() {
                        current.push(Span::styled(
                            "│ ".to_string(),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    current.push(Span::styled(
                        text.to_string(),
                        md_style(base_style, bold, italic, in_link),
                    ));
                }
            }
            Event::SoftBreak => {
                if !in_code_block {
                    current.push(Span::raw(" "));
                }
            }
            Event::HardBreak => {
                md_flush(&mut result, &mut current);
            }
            Event::Rule => {
                md_flush(&mut result, &mut current);
                result.push(PageLine::Spans(vec![Span::styled(
                    "─".repeat(60),
                    Style::default().fg(Color::DarkGray),
                )]));
                result.push(PageLine::Blank);
            }
            _ => {}
        }
    }
    md_flush(&mut result, &mut current);
    (result, images)
}

pub fn page_line_plain_text(line: &PageLine) -> String {
    match line {
        PageLine::Spans(spans) => spans.iter().map(|s| s.content.as_ref()).collect(),
        PageLine::Blank => String::new(),
        PageLine::Image { .. } => String::new(),
    }
}

pub fn find_page_search_matches(lines: &[PageLine], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return vec![];
    }
    let q = query.to_ascii_lowercase();
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| page_line_plain_text(l).to_ascii_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

enum ConfluenceData {
    Spaces(Vec<jui_core::confluence_api::ConfluenceSpace>),
    Pages(Vec<jui_core::confluence_api::ConfluencePage>),
}

/// Hit the daemon IPC; return `None` if daemon is unavailable or returns unexpected response.
async fn try_ipc_for_confluence(req: &ipc::Request) -> Option<ConfluenceData> {
    use jui_core::ipc::Response;
    let mut stream = ipc::connect().await.ok()?;
    match ipc::send_request(&mut stream, req).await.ok()? {
        Response::ConfluenceSpaces { items, .. } => Some(ConfluenceData::Spaces(items)),
        Response::ConfluencePages { items, .. } => Some(ConfluenceData::Pages(items)),
        _ => None,
    }
}

async fn download_confluence_images(
    html: &str,
    server: &str,
    login: &str,
    token: &str,
    page_id: &str,
) -> String {
    let dir = format!("/tmp/confluence-assets/{}", page_id);
    let _ = std::fs::create_dir_all(&dir);

    let mut replacements: Vec<(String, String)> = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;

    while pos < lower.len() {
        let Some(rel) = lower[pos..].find("<img") else {
            break;
        };
        let abs = pos + rel;
        let after = abs + 4;
        let tag_end = lower[after..]
            .find('>')
            .map(|e| after + e + 1)
            .unwrap_or(lower.len());
        let tag = &html[abs..tag_end];

        if let Some(src) = extract_attr(
            tag.trim_start_matches('<')
                .trim_start_matches("img")
                .trim_start_matches("IMG"),
            "src",
        ) {
            let is_relative = src.starts_with("/wiki/") || src.starts_with("/download/");
            let is_absolute = src.starts_with(server)
                && (src[server.len()..].starts_with("/wiki/")
                    || src[server.len()..].starts_with("/download/"));
            if (is_relative || is_absolute) && !replacements.iter().any(|(o, _)| o == &src) {
                let full_url = if is_absolute {
                    src.clone()
                } else {
                    format!("{}{}", server, src)
                };
                let filename = src
                    .split('/')
                    .last()
                    .and_then(|f| f.split('?').next())
                    .filter(|f| !f.is_empty())
                    .unwrap_or("image.png");
                let local_path = format!("{}/{}", dir, filename);

                let ok = tokio::process::Command::new("curl")
                    .args([
                        "-sS",
                        "-L",
                        "--fail-with-body",
                        "-u",
                        &format!("{}:{}", login, token),
                        "-o",
                        &local_path,
                        &full_url,
                    ])
                    .output()
                    .await
                    .map(|o| o.status.success())
                    .unwrap_or(false);

                if ok {
                    replacements.push((src, format!("file://{}", local_path)));
                }
            }
        }
        pos = tag_end;
    }

    let mut result = html.to_string();
    for (original, local) in replacements {
        result = result.replace(&original, &local);
    }
    result
}

fn html_to_markdown(html: &str) -> String {
    use std::io::Write;
    // Try pandoc first.
    if let Ok(mut child) = std::process::Command::new("pandoc")
        .args(["-f", "html", "-t", "markdown", "--wrap=none"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(html.as_bytes());
        }
        if let Ok(out) = child.wait_with_output() {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).into_owned();
            }
        }
    }
    // Try python3 html2text (third-party package).
    if let Ok(mut child) = std::process::Command::new("python3")
        .args([
            "-c",
            "import sys,html2text; print(html2text.html2text(sys.stdin.read()))",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(html.as_bytes());
        }
        if let Ok(out) = child.wait_with_output() {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).into_owned();
            }
        }
    }
    // Built-in converter: handles Confluence export_view HTML well enough.
    html_to_md_builtin(html)
}

fn html_to_md_builtin(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    let chars: Vec<char> = html.chars().collect();
    let len = chars.len();

    // Ordered-list counter stack: each entry is Some(n) for <ol> or None for <ul>.
    let mut list_stack: Vec<Option<usize>> = Vec::new();
    let mut href_stack: Vec<String> = Vec::new();
    let mut in_pre = false;
    let mut skip = false; // inside <script>/<style>

    while i < len {
        if chars[i] == '<' {
            // Collect the tag.
            let start = i + 1;
            i += 1;
            while i < len && chars[i] != '>' {
                i += 1;
            }
            let raw_tag: String = chars[start..i].iter().collect();
            i += 1; // skip '>'

            let closing = raw_tag.starts_with('/');
            let tag_body = raw_tag.trim_start_matches('/').trim();
            let tag_name = tag_body
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();

            // skip script / style content
            if tag_name == "script" || tag_name == "style" {
                skip = !closing;
                continue;
            }
            if skip {
                continue;
            }

            match tag_name.as_str() {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if !closing => {
                    let level = tag_name[1..].parse::<usize>().unwrap_or(1);
                    ensure_blank_line(&mut out);
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if closing => {
                    out.push('\n');
                }
                "p" | "div" | "section" | "article" | "header" | "footer" if !closing => {
                    ensure_blank_line(&mut out);
                }
                "p" | "div" | "section" | "article" | "header" | "footer" if closing => {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                "br" => {
                    out.push('\n');
                }
                "hr" => {
                    ensure_blank_line(&mut out);
                    out.push_str("---\n");
                }
                "ul" if !closing => {
                    list_stack.push(None);
                    ensure_newline(&mut out);
                }
                "ol" if !closing => {
                    list_stack.push(Some(0));
                    ensure_newline(&mut out);
                }
                "ul" | "ol" if closing => {
                    list_stack.pop();
                    ensure_newline(&mut out);
                }
                "li" if !closing => {
                    ensure_newline(&mut out);
                    let depth = list_stack.len().saturating_sub(1);
                    out.push_str(&"  ".repeat(depth));
                    if let Some(Some(n)) = list_stack.last_mut() {
                        *n += 1;
                        out.push_str(&format!("{}. ", n));
                    } else {
                        out.push_str("- ");
                    }
                }
                "li" if closing => {
                    ensure_newline(&mut out);
                }
                "strong" | "b" => {
                    out.push_str("**");
                }
                "em" | "i" => {
                    out.push('*');
                }
                "code" if !in_pre => {
                    out.push('`');
                }
                "pre" if !closing => {
                    ensure_blank_line(&mut out);
                    out.push_str("```\n");
                    in_pre = true;
                }
                "pre" if closing => {
                    ensure_newline(&mut out);
                    out.push_str("```\n");
                    in_pre = false;
                }
                "a" if !closing => {
                    if let Some(href) = extract_attr(tag_body, "href") {
                        out.push('[');
                        href_stack.push(href);
                    }
                }
                "a" if closing => {
                    if let Some(href) = href_stack.pop() {
                        out.push_str(&format!("]({})", href));
                    }
                }
                "img" => {
                    let src = extract_attr(tag_body, "src").unwrap_or_default();
                    let alt = extract_attr(tag_body, "alt").unwrap_or_default();
                    if !src.is_empty() {
                        ensure_blank_line(&mut out);
                        out.push_str(&format!("![{}]({})\n", alt, src));
                    }
                }
                "blockquote" if !closing => {
                    ensure_blank_line(&mut out);
                    out.push_str("> ");
                }
                "table" | "tbody" | "thead" if !closing => {
                    ensure_blank_line(&mut out);
                }
                "tr" if closing => {
                    out.push_str(" |\n");
                }
                "td" | "th" if !closing => {
                    out.push_str("| ");
                }
                _ => {}
            }
        } else {
            // Text node.
            if skip {
                i += 1;
                continue;
            }
            let mut text = String::new();
            while i < len && chars[i] != '<' {
                text.push(chars[i]);
                i += 1;
            }
            let decoded = decode_entities(&text);
            if in_pre {
                out.push_str(&decoded);
            } else {
                // Collapse whitespace, but preserve newlines after block starts.
                let collapsed = decoded
                    .split(|c: char| c == '\n' || c == '\r')
                    .flat_map(|line| {
                        let t = line.split_whitespace().collect::<Vec<_>>().join(" ");
                        std::iter::once(t)
                    })
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !collapsed.is_empty() {
                    // Add space between last text and new text if needed.
                    if !out.is_empty()
                        && !out.ends_with('\n')
                        && !out.ends_with(' ')
                        && !out.ends_with('#')
                        && !out.ends_with('-')
                        && !out.ends_with('>')
                        && !out.ends_with('`')
                        && !out.ends_with('[')
                        && !out.ends_with('*')
                    {
                        out.push(' ');
                    }
                    out.push_str(&collapsed);
                }
            }
        }
    }

    // Collapse 3+ consecutive newlines to 2.
    let mut result = String::with_capacity(out.len());
    let mut newline_count = 0usize;
    for ch in out.chars() {
        if ch == '\n' {
            newline_count += 1;
            if newline_count <= 2 {
                result.push(ch);
            }
        } else {
            newline_count = 0;
            result.push(ch);
        }
    }
    result.trim().to_string()
}

fn ensure_blank_line(out: &mut String) {
    if out.is_empty() {
        return;
    }
    if !out.ends_with("\n\n") {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
}

fn ensure_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = s[i..].find(';') {
                let entity = &s[i + 1..i + semi];
                let replacement = match entity {
                    "lt" => "<",
                    "gt" => ">",
                    "amp" => "&",
                    "nbsp" | "#160" => " ",
                    "quot" => "\"",
                    "apos" => "'",
                    "mdash" | "#8212" => "—",
                    "ndash" | "#8211" => "–",
                    "hellip" | "#8230" => "…",
                    "laquo" | "#171" => "«",
                    "raquo" | "#187" => "»",
                    e if e.starts_with('#') => {
                        let n: u32 = e[1..].parse().unwrap_or(0);
                        let ch = char::from_u32(n).unwrap_or(' ');
                        out.push(ch);
                        i += semi + 1;
                        continue;
                    }
                    _ => {
                        out.push('&');
                        i += 1;
                        continue;
                    }
                };
                out.push_str(replacement);
                i += semi + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn extract_attr<'a>(tag_body: &'a str, attr: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{}={}", attr, quote);
        if let Some(pos) = tag_body.find(&needle) {
            let start = pos + needle.len();
            if let Some(end_rel) = tag_body[start..].find(quote) {
                return Some(tag_body[start..start + end_rel].to_string());
            }
        }
    }
    None
}

pub async fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let mut app = App::new();
    if let Err(e) = app.load_myself().await {
        app.status = format!("auth lookup failed: {e:#}");
    }
    // Background-warm the ticket cache so list-view is ready when the user
    // navigates into it from Home. Failures are non-fatal — Home renders
    // even when Jira's unreachable.
    if let Err(e) = app.refresh().await {
        app.status = format!("refresh failed: {e:#}");
    }
    if let Err(e) = app.load_home_activity().await {
        app.status = format!("activity load failed: {e:#}");
    }

    let res = main_loop(&mut term, &mut app).await;

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

async fn main_loop<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    loop {
        if app.needs_clear {
            term.clear()?;
            app.needs_clear = false;
        }
        // Keep page viewer viewport in sync with actual terminal height.
        if let Mode::PageView(ref mut form) = app.mode {
            if let Ok(sz) = term.size() {
                form.viewport_height = sz.height.saturating_sub(5) as usize;
            }
        }
        // Drain any background "claude tighten" task that finished. The
        // 200ms poll cadence below is enough — no need to shorten it.
        app.poll_pending_improve();
        app.poll_pending_pr_review();
        app.poll_pending_pr_body_improve();
        app.poll_copilot_fix_job();
        // Animate the spinner once per draw tick.
        app.spinner_tick = app.spinner_tick.wrapping_add(1);
        term.draw(|f| ui::draw(f, app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                handle_key(app, k.code, k.modifiers).await?;
                // Note: List/Archive intentionally don't auto-refresh on mode
                // transition any more — the daemon cache is kept hot via the
                // poll loop and via async refresh-after-mutation, so the user
                // only pays the Jira round-trip when they explicitly press 'r'
                // or change something.
            }
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

async fn settings_keys(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Result<()> {
    // Picker overlay first — when active, only it gets keys.
    let picker_open = matches!(&app.mode, Mode::Settings(f) if f.picker.is_some());
    if picker_open {
        // Snapshot the commit info while holding the borrow, then apply commit
        // after dropping it (save_settings re-borrows app).
        let mut commit: Option<(usize, String)> = None;
        if let Mode::Settings(form) = &mut app.mode {
            let Some(p) = form.picker.as_mut() else {
                return Ok(());
            };
            match code {
                KeyCode::Esc => {
                    form.picker = None;
                    app.status = "pick cancelled".into();
                    return Ok(());
                }
                KeyCode::Down => {
                    let n = p.filtered().len();
                    if n > 0 {
                        p.selected = (p.selected + 1).min(n - 1);
                    }
                    return Ok(());
                }
                KeyCode::Up => {
                    p.selected = p.selected.saturating_sub(1);
                    return Ok(());
                }
                KeyCode::Backspace => {
                    p.query.pop();
                    p.selected = 0;
                    return Ok(());
                }
                KeyCode::Enter => {
                    let val = p.filtered().get(p.selected).map(|s| s.to_string());
                    let Some(val) = val else {
                        app.status = "no matching status".into();
                        return Ok(());
                    };
                    commit = Some((p.row, val));
                }
                KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                    p.query.push(c);
                    p.selected = 0;
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        if let Some((row, val)) = commit {
            if let Mode::Settings(form) = &mut app.mode {
                form.picker = None;
                let label = match row {
                    0 => {
                        form.default_create_status = val.clone();
                        "default-create"
                    }
                    1 => {
                        form.all_mine_exclude_status = val.clone();
                        "all-mine-exclude"
                    }
                    2 => {
                        form.pr_submit_status = val.clone();
                        "pr-submit"
                    }
                    3 => {
                        form.code_assistant = val.clone();
                        "code-assistant"
                    }
                    4 => {
                        form.claude_permission_mode = val.clone();
                        "claude-permission-mode"
                    }
                    _ => "",
                };
                let l = label.to_string();
                if let Err(e) = app.save_settings() {
                    app.status = format!("save err: {e:#}");
                } else {
                    app.status = format!("{l} = \"{val}\"");
                }
            }
            return Ok(());
        }
        return Ok(());
    }
    // Navigation mode (picker closed).
    let mut open_picker = false;
    if matches!(&app.mode, Mode::Settings(_)) && matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
        app.pop_back_or_quit().await?;
        return Ok(());
    }
    if let Mode::Settings(form) = &mut app.mode {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Ok(());
            }
            KeyCode::Char('j') | KeyCode::Down => {
                form.selected = (form.selected + 1).min(SettingsForm::ROW_COUNT - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected = form.selected.saturating_sub(1);
            }
            KeyCode::Char('i') | KeyCode::Enter => {
                open_picker = true;
            }
            _ => {}
        }
    }
    if open_picker {
        app.open_settings_picker().await?;
    }
    Ok(())
}

async fn rules_keys(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> Result<()> {
    let mut open_edit_idx: Option<usize> = None;
    let mut open_edit_new = false;
    let mut do_save = false;
    let mut open_log = false;
    // Esc/q from the rules pane pops the nav stack (back to whatever
    // brought us here — typically Home). Handle early so the async call
    // doesn't fight the borrow on `app.mode` below.
    if matches!(&app.mode, Mode::Rules(_)) && matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
        app.pop_back_or_quit().await?;
        return Ok(());
    }
    if let Mode::Rules(form) = &mut app.mode {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                return Ok(());
            }
            KeyCode::Char('l') => {
                open_log = true;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.items.is_empty() {
                    form.selected = (form.selected + 1).min(form.items.len() - 1);
                }
                form.pending_remove = None;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected = form.selected.saturating_sub(1);
                form.pending_remove = None;
            }
            KeyCode::Char('a') => {
                open_edit_new = true;
            }
            KeyCode::Char('t') => {
                if let Some(r) = form.items.get_mut(form.selected) {
                    r.enabled = !r.enabled;
                    do_save = true;
                }
            }
            KeyCode::Char('d') => {
                if form.pending_remove == Some(form.selected) {
                    if form.selected < form.items.len() {
                        form.items.remove(form.selected);
                        form.selected = form.selected.min(form.items.len().saturating_sub(1));
                        do_save = true;
                    }
                    form.pending_remove = None;
                } else {
                    form.pending_remove = Some(form.selected);
                    app.status = "press d again to confirm delete".into();
                }
            }
            KeyCode::Enter => {
                if !form.items.is_empty() {
                    open_edit_idx = Some(form.selected);
                }
            }
            _ => {}
        }
    }
    if do_save {
        if let Err(e) = app.save_rules() {
            app.status = format!("save err: {e:#}");
        } else {
            app.status = "rules saved".into();
        }
    }
    if open_edit_new {
        app.push_current_view();
        let rule = jui_core::rules::Rule {
            id: jui_core::rules::new_rule_id(),
            name: "new rule".into(),
            enabled: true,
            trigger: jui_core::rules::Trigger::PrCreated,
            conditions: vec![],
            actions: vec![],
        };
        app.mode = Mode::RuleEdit(RuleEditForm {
            original_id: None,
            rule,
            selected_row: 0,
            edit_buffer: None,
            edit_target: None,
            pending_remove_condition: None,
            pending_remove_action: None,
            error: None,
            picker: None,
            var_picker: None,
        });
        app.status =
            "new rule — j/k move · enter edit · a add cond/act · ctrl-s save · esc cancel".into();
    } else if let Some(idx) = open_edit_idx {
        let snapshot = if let Mode::Rules(form) = &app.mode {
            form.items.get(idx).cloned()
        } else {
            None
        };
        if let Some(rule) = snapshot {
            app.push_current_view();
            let id = rule.id.clone();
            app.mode = Mode::RuleEdit(RuleEditForm {
                original_id: Some(id),
                rule,
                selected_row: 0,
                edit_buffer: None,
                edit_target: None,
                pending_remove_condition: None,
                pending_remove_action: None,
                error: None,
                picker: None,
                var_picker: None,
            });
            app.status = "editing rule — ctrl-s save · esc cancel".into();
        }
    }
    if open_log {
        app.push_current_view();
        app.open_rule_log().await?;
    }
    Ok(())
}

async fn home_keys(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> Result<()> {
    let mut do_refresh = false;
    let mut open_target: Option<HomeTarget> = None;
    let mut open_ticket: Option<String> = None;
    if let Mode::Home(form) = &mut app.mode {
        // Number-key shortcuts always work regardless of focus — quicker than
        // moving the cursor.
        if let KeyCode::Char(c) = code {
            for (i, (target, num, hot, _desc)) in HOME_TARGETS.iter().enumerate() {
                if c.to_string() == *num
                    || c.eq_ignore_ascii_case(&hot.chars().next().unwrap_or('\0'))
                {
                    open_target = Some(*target);
                    form.menu_selected = i;
                    break;
                }
            }
        }
        if open_target.is_none() {
            match code {
                KeyCode::Char('q') => {
                    app.should_quit = true;
                    return Ok(());
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    form.focus = match form.focus {
                        HomeFocus::Menu => HomeFocus::Activity,
                        HomeFocus::Activity => HomeFocus::Menu,
                    };
                }
                KeyCode::Char('r') => do_refresh = true,
                KeyCode::Char('j') | KeyCode::Down => match form.focus {
                    HomeFocus::Menu => {
                        if !HOME_TARGETS.is_empty() {
                            form.menu_selected =
                                (form.menu_selected + 1).min(HOME_TARGETS.len() - 1);
                        }
                    }
                    HomeFocus::Activity => {
                        if !form.items.is_empty() {
                            form.selected = (form.selected + 1).min(form.items.len() - 1);
                        }
                    }
                },
                KeyCode::Char('k') | KeyCode::Up => match form.focus {
                    HomeFocus::Menu => {
                        form.menu_selected = form.menu_selected.saturating_sub(1);
                    }
                    HomeFocus::Activity => {
                        form.selected = form.selected.saturating_sub(1);
                    }
                },
                KeyCode::Enter => match form.focus {
                    HomeFocus::Menu => {
                        if let Some((target, _, _, _)) = HOME_TARGETS.get(form.menu_selected) {
                            open_target = Some(*target);
                        }
                    }
                    HomeFocus::Activity => {
                        if let Some(item) = form.items.get(form.selected) {
                            if let Some(k) = &item.ticket_key {
                                open_ticket = Some(k.clone());
                            }
                        }
                    }
                },
                _ => {}
            }
        }
    }
    if do_refresh {
        app.load_home_activity().await?;
    }
    if let Some(target) = open_target {
        app.home_open(target).await?;
    } else if let Some(key) = open_ticket {
        // Opening a Detail from the Home activity feed counts as forward
        // navigation; push Home so Q from Detail pops back here.
        app.push_current_view();
        app.detail_origin = DetailOrigin::List;
        app.open_ticket_by_key(key).await?;
        app.mode = Mode::Detail;
    }
    Ok(())
}

async fn rule_log_keys(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> Result<()> {
    let mut do_refresh = false;
    if let Mode::RuleLog(form) = &mut app.mode {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.open_rules();
                return Ok(());
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.items.is_empty() {
                    form.selected = (form.selected + 1).min(form.items.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected = form.selected.saturating_sub(1);
            }
            KeyCode::Char('g') => form.selected = 0,
            KeyCode::Char('G') => {
                form.selected = form.items.len().saturating_sub(1);
            }
            KeyCode::PageDown => {
                form.selected = (form.selected + 10).min(form.items.len().saturating_sub(1));
            }
            KeyCode::PageUp => {
                form.selected = form.selected.saturating_sub(10);
            }
            KeyCode::Char('r') => do_refresh = true,
            _ => {}
        }
    }
    if do_refresh {
        app.open_rule_log().await?;
    }
    Ok(())
}

async fn pull_requests_keys(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> Result<()> {
    let mut refresh = false;
    if let Mode::PullRequests(form) = &mut app.mode {
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.items.is_empty() {
                    form.selected = (form.selected + 1).min(form.items.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected = form.selected.saturating_sub(1);
            }
            KeyCode::Char('g') => form.selected = 0,
            KeyCode::Char('G') => form.selected = form.items.len().saturating_sub(1),
            KeyCode::PageDown => {
                form.selected = (form.selected + 10).min(form.items.len().saturating_sub(1));
            }
            KeyCode::PageUp => form.selected = form.selected.saturating_sub(10),
            KeyCode::Char('r') => refresh = true,
            KeyCode::Enter => app.open_selected_pull_request_ticket().await?,
            KeyCode::Char('F') | KeyCode::Char('f') => {
                app.launch_copilot_fix_for_selected_pr_in_app()?
            }
            _ => {}
        }
    }
    if refresh {
        app.refresh_pull_requests().await?;
    }
    Ok(())
}

async fn copilot_fix_keys(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> Result<()> {
    match code {
        KeyCode::Char('K') | KeyCode::Char('k') => app.kill_copilot_fix_job(),
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('b') => {
            app.background_copilot_fix_job().await?;
        }
        KeyCode::PageUp => {
            if let Mode::CopilotFixRun(form) = &mut app.mode {
                form.scroll_from_bottom = form.scroll_from_bottom.saturating_add(10);
            }
        }
        KeyCode::PageDown => {
            if let Mode::CopilotFixRun(form) = &mut app.mode {
                form.scroll_from_bottom = form.scroll_from_bottom.saturating_sub(10);
            }
        }
        KeyCode::Home | KeyCode::Char('g') => {
            if let Mode::CopilotFixRun(form) = &mut app.mode {
                let lines = app
                    .copilot_fix_job
                    .as_ref()
                    .map(|job| job.output.len())
                    .unwrap_or(0);
                form.scroll_from_bottom = lines;
            }
        }
        KeyCode::End | KeyCode::Char('G') => {
            if let Mode::CopilotFixRun(form) = &mut app.mode {
                form.scroll_from_bottom = 0;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Dynamic row layout for the rule editor. The order is:
///   Name, Trigger, [TriggerFilter when applicable], Enabled,
///   ConditionsHeader, [Cond(0..N)], AddCondition,
///   ActionsHeader,    [Act(0..M)],  AddAction
#[derive(Debug, Clone, Copy)]
pub enum RuleRow {
    Name,
    Trigger,
    TriggerFilter,
    Enabled,
    ConditionsHeader,
    Cond(usize),
    AddCondition,
    ActionsHeader,
    Act(usize),
    AddAction,
}

pub fn rule_edit_rows(rule: &jui_core::rules::Rule) -> Vec<RuleRow> {
    let mut out = vec![RuleRow::Name, RuleRow::Trigger];
    if trigger_has_filter(&rule.trigger) {
        out.push(RuleRow::TriggerFilter);
    }
    out.push(RuleRow::Enabled);
    out.push(RuleRow::ConditionsHeader);
    for i in 0..rule.conditions.len() {
        out.push(RuleRow::Cond(i));
    }
    out.push(RuleRow::AddCondition);
    out.push(RuleRow::ActionsHeader);
    for i in 0..rule.actions.len() {
        out.push(RuleRow::Act(i));
    }
    out.push(RuleRow::AddAction);
    out
}

fn trigger_has_filter(t: &jui_core::rules::Trigger) -> bool {
    matches!(
        t,
        jui_core::rules::Trigger::TicketStatusChanged { .. }
            | jui_core::rules::Trigger::TicketAssigned { .. }
    )
}

/// Cycle through trigger variants for the cycle-picker key on the Trigger row.
pub fn cycle_trigger(t: &jui_core::rules::Trigger, forward: bool) -> jui_core::rules::Trigger {
    use jui_core::rules::Trigger as T;
    let variants = [
        T::PrCreated,
        T::StartWork,
        T::StopWork,
        T::TicketStatusChanged {
            from: None,
            to: None,
        },
        T::TicketAssigned { to_me: None },
    ];
    let i = match t {
        T::PrCreated => 0,
        T::StartWork => 1,
        T::StopWork => 2,
        T::TicketStatusChanged { .. } => 3,
        T::TicketAssigned { .. } => 4,
    };
    let n = variants.len();
    let next = if forward {
        (i + 1) % n
    } else {
        (i + n - 1) % n
    };
    variants[next].clone()
}

fn cycle_condition(c: &jui_core::rules::Condition) -> jui_core::rules::Condition {
    use jui_core::rules::Condition as C;
    match c {
        C::ProjectKeyEquals { .. } => C::StatusEquals {
            value: String::new(),
        },
        C::StatusEquals { .. } => C::IssueTypeIn { values: vec![] },
        C::IssueTypeIn { .. } => C::HasLinkedRepo,
        C::HasLinkedRepo => C::ActorIsMe,
        C::ActorIsMe => C::ProjectKeyEquals {
            value: String::new(),
        },
    }
}

fn cycle_action(a: &jui_core::rules::Action) -> jui_core::rules::Action {
    use jui_core::rules::Action as A;
    match a {
        A::JiraTransition { .. } => A::JiraComment {
            body: String::new(),
        },
        A::JiraComment { .. } => A::GithubPrComment {
            body: String::new(),
        },
        A::GithubPrComment { .. } => A::SetTicketDevQa,
        A::SetTicketDevQa => A::JiraTransition { to: String::new() },
    }
}

async fn rule_edit_keys(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Result<()> {
    // Picker overlay first — when open, all keys feed it.
    if matches!(&app.mode, Mode::RuleEdit(f) if f.picker.is_some()) {
        return rule_edit_picker_keys(app, code).await;
    }
    // Compute the row layout once per keypress.
    let rows: Vec<RuleRow> = if let Mode::RuleEdit(form) = &app.mode {
        rule_edit_rows(&form.rule)
    } else {
        return Ok(());
    };
    let n = rows.len();
    let mut do_save = false;
    let mut bail = false;
    let mut open_picker: Option<RulePickerTarget> = None;
    if let Mode::RuleEdit(form) = &mut app.mode {
        // Text-input mode takes precedence: keystrokes feed the buffer until
        // Enter (commit) or Esc (abort).
        if let Some(buf) = form.edit_buffer.as_mut() {
            // Variable-autocomplete sub-state: open when the user typed `{`.
            // Filter chars typed since the `{` form the live filter; Tab /
            // Enter while the picker is open insert the selected variable
            // and keep edit mode open, so the user can keep typing and then
            // press Enter again to commit the whole field.
            if let Some(vp) = form.var_picker.as_mut() {
                let filter: String = buf[vp.anchor + 1..].to_string();
                let matches_len = filter_vars(&filter).len();
                match code {
                    KeyCode::Esc => {
                        form.var_picker = None;
                    }
                    KeyCode::Up => {
                        vp.selected = vp.selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if matches_len > 0 {
                            vp.selected = (vp.selected + 1).min(matches_len - 1);
                        }
                    }
                    KeyCode::Tab | KeyCode::Enter => {
                        let matches = filter_vars(&filter);
                        let picked = matches
                            .get(vp.selected.min(matches.len().saturating_sub(1)))
                            .map(|(name, _)| (*name).to_string());
                        if let Some(name) = picked {
                            buf.truncate(vp.anchor);
                            buf.push('{');
                            buf.push_str(&name);
                            buf.push('}');
                        }
                        form.var_picker = None;
                    }
                    KeyCode::Backspace => {
                        if buf.len() > vp.anchor + 1 {
                            buf.pop();
                            vp.selected = 0;
                        } else {
                            // Past the `{` — pop it too and close picker.
                            buf.pop();
                            form.var_picker = None;
                        }
                    }
                    KeyCode::Char(c) => {
                        if c == '}' {
                            // User typed the closing brace themselves —
                            // commit whatever they have literally and stop.
                            buf.push(c);
                            form.var_picker = None;
                        } else if c == '{' {
                            // Reopen at a new anchor (e.g. inside an already-
                            // closed brace pair).
                            vp.anchor = buf.len();
                            vp.selected = 0;
                            buf.push(c);
                        } else {
                            buf.push(c);
                            vp.selected = 0;
                        }
                    }
                    _ => {}
                }
                return Ok(());
            }
            match code {
                KeyCode::Esc => {
                    form.edit_buffer = None;
                    form.edit_target = None;
                }
                KeyCode::Enter => {
                    let value = buf.clone();
                    let target = form.edit_target.clone();
                    form.edit_buffer = None;
                    form.edit_target = None;
                    apply_edit(&mut form.rule, target, value);
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char('{') => {
                    let anchor = buf.len();
                    buf.push('{');
                    form.var_picker = Some(VarPicker {
                        anchor,
                        selected: 0,
                    });
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                }
                _ => {}
            }
            return Ok(());
        }
        match code {
            KeyCode::Esc => {
                bail = true;
            }
            KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => {
                do_save = true;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                form.selected_row = (form.selected_row + 1).min(n.saturating_sub(1));
                form.pending_remove_condition = None;
                form.pending_remove_action = None;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected_row = form.selected_row.saturating_sub(1);
                form.pending_remove_condition = None;
                form.pending_remove_action = None;
            }
            KeyCode::Char(' ') | KeyCode::Tab | KeyCode::Right => {
                // Cycle whatever is selected.
                match rows.get(form.selected_row).copied() {
                    Some(RuleRow::Trigger) => {
                        form.rule.trigger = cycle_trigger(&form.rule.trigger, true);
                    }
                    Some(RuleRow::Enabled) => {
                        form.rule.enabled = !form.rule.enabled;
                    }
                    Some(RuleRow::Cond(i)) => {
                        if let Some(c) = form.rule.conditions.get_mut(i) {
                            *c = cycle_condition(c);
                        }
                    }
                    Some(RuleRow::Act(i)) => {
                        if let Some(a) = form.rule.actions.get_mut(i) {
                            *a = cycle_action(a);
                        }
                    }
                    _ => {}
                }
            }
            KeyCode::BackTab | KeyCode::Left => {
                if let Some(RuleRow::Trigger) = rows.get(form.selected_row).copied() {
                    form.rule.trigger = cycle_trigger(&form.rule.trigger, false);
                }
            }
            KeyCode::Char('e') | KeyCode::Enter => {
                // Open text-edit buffer for whichever row supports it.
                match rows.get(form.selected_row).copied() {
                    Some(RuleRow::Name) => {
                        form.edit_buffer = Some(form.rule.name.clone());
                        form.edit_target = Some(RuleEditTarget::Name);
                    }
                    Some(RuleRow::TriggerFilter) => {
                        let cur = match &form.rule.trigger {
                            jui_core::rules::Trigger::TicketStatusChanged { to, .. } => {
                                to.clone().unwrap_or_default()
                            }
                            jui_core::rules::Trigger::TicketAssigned { to_me } => {
                                to_me.map(|b| b.to_string()).unwrap_or_default()
                            }
                            _ => String::new(),
                        };
                        form.edit_buffer = Some(cur);
                        form.edit_target = Some(RuleEditTarget::TriggerFilter);
                    }
                    Some(RuleRow::Cond(i)) => {
                        let cur = match form.rule.conditions.get(i) {
                            Some(jui_core::rules::Condition::ProjectKeyEquals { value }) => {
                                value.clone()
                            }
                            Some(jui_core::rules::Condition::StatusEquals { value }) => {
                                value.clone()
                            }
                            Some(jui_core::rules::Condition::IssueTypeIn { values }) => {
                                values.join(",")
                            }
                            _ => String::new(),
                        };
                        form.edit_buffer = Some(cur);
                        form.edit_target = Some(RuleEditTarget::ConditionValue(i));
                    }
                    Some(RuleRow::Act(i)) => {
                        // JiraTransition pops a status picker (filter list of
                        // all statuses); other text actions open the inline
                        // edit buffer; SetTicketDevQa has nothing to edit.
                        match form.rule.actions.get(i) {
                            Some(jui_core::rules::Action::JiraTransition { .. }) => {
                                open_picker = Some(RulePickerTarget::ActionTransitionTo(i));
                            }
                            Some(jui_core::rules::Action::JiraComment { body }) => {
                                form.edit_buffer = Some(body.clone());
                                form.edit_target = Some(RuleEditTarget::ActionValue(i));
                            }
                            Some(jui_core::rules::Action::GithubPrComment { body }) => {
                                form.edit_buffer = Some(body.clone());
                                form.edit_target = Some(RuleEditTarget::ActionValue(i));
                            }
                            Some(jui_core::rules::Action::SetTicketDevQa) | None => {
                                app.status = "this action has no editable value — use tab/space to cycle action kind".into();
                            }
                        }
                    }
                    Some(RuleRow::AddCondition) => {
                        form.rule
                            .conditions
                            .push(jui_core::rules::Condition::HasLinkedRepo);
                    }
                    Some(RuleRow::AddAction) => {
                        form.rule
                            .actions
                            .push(jui_core::rules::Action::JiraComment {
                                body: String::new(),
                            });
                    }
                    _ => {}
                }
            }
            KeyCode::Char('a') => {
                // Same as add on AddCondition / AddAction rows, but also
                // works from header rows for convenience.
                match rows.get(form.selected_row).copied() {
                    Some(RuleRow::ConditionsHeader)
                    | Some(RuleRow::AddCondition)
                    | Some(RuleRow::Cond(_)) => {
                        form.rule
                            .conditions
                            .push(jui_core::rules::Condition::HasLinkedRepo);
                    }
                    Some(RuleRow::ActionsHeader)
                    | Some(RuleRow::AddAction)
                    | Some(RuleRow::Act(_)) => {
                        form.rule
                            .actions
                            .push(jui_core::rules::Action::JiraComment {
                                body: String::new(),
                            });
                    }
                    _ => {}
                }
            }
            KeyCode::Char('d') => match rows.get(form.selected_row).copied() {
                Some(RuleRow::Cond(i)) => {
                    if form.pending_remove_condition == Some(i) {
                        if i < form.rule.conditions.len() {
                            form.rule.conditions.remove(i);
                        }
                        form.pending_remove_condition = None;
                    } else {
                        form.pending_remove_condition = Some(i);
                        app.status = "press d again to delete condition".into();
                    }
                }
                Some(RuleRow::Act(i)) => {
                    if form.pending_remove_action == Some(i) {
                        if i < form.rule.actions.len() {
                            form.rule.actions.remove(i);
                        }
                        form.pending_remove_action = None;
                    } else {
                        form.pending_remove_action = Some(i);
                        app.status = "press d again to delete action".into();
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    if bail {
        app.open_rules();
        return Ok(());
    }
    if do_save {
        if let Err(e) = app.save_rule_edit() {
            if let Mode::RuleEdit(f) = &mut app.mode {
                f.error = Some(format!("save err: {e:#}"));
            }
        }
    }
    if let Some(target) = open_picker {
        app.open_rule_picker(target).await?;
    }
    Ok(())
}

/// Open the status picker for a rule-edit field. Currently only fired for
/// `JiraTransition.to`. Async because we hit the daemon for the full Jira
/// status list — same `ListStatuses` request the Settings picker uses.
impl App {
    pub async fn open_rule_picker(&mut self, target: RulePickerTarget) -> Result<()> {
        // Pre-select the current value if any (for ActionTransitionTo).
        let current = match (&self.mode, target) {
            (Mode::RuleEdit(f), RulePickerTarget::ActionTransitionTo(i)) => {
                match f.rule.actions.get(i) {
                    Some(jui_core::rules::Action::JiraTransition { to }) => to.clone(),
                    _ => String::new(),
                }
            }
            _ => String::new(),
        };
        if let Mode::RuleEdit(f) = &mut self.mode {
            f.picker = Some(RulePicker {
                target,
                query: String::new(),
                all: Vec::new(),
                selected: 0,
                loading: true,
                error: None,
            });
        }
        self.status = "fetching statuses…".into();
        let resp = {
            let mut s = ipc::connect().await?;
            ipc::send_request(&mut s, &Request::ListStatuses).await?
        };
        if let Mode::RuleEdit(f) = &mut self.mode {
            let Some(p) = f.picker.as_mut() else {
                return Ok(());
            };
            p.loading = false;
            match resp {
                Response::Statuses { items } => {
                    if let Some(pos) = items.iter().position(|s| s.eq_ignore_ascii_case(&current)) {
                        p.selected = pos;
                    }
                    p.all = items;
                    self.status = "type: filter · j/k: move · enter: pick · esc: cancel".into();
                }
                Response::Err { message } => {
                    p.error = Some(message.clone());
                    self.status = format!("status fetch failed: {message}");
                }
                _ => {
                    p.error = Some("unexpected response".into());
                    self.status = "unexpected response".into();
                }
            }
        }
        Ok(())
    }
}

async fn rule_edit_picker_keys(app: &mut App, code: KeyCode) -> Result<()> {
    let mut commit: Option<(RulePickerTarget, String)> = None;
    if let Mode::RuleEdit(form) = &mut app.mode {
        let Some(p) = form.picker.as_mut() else {
            return Ok(());
        };
        match code {
            KeyCode::Esc => {
                form.picker = None;
                app.status = "pick cancelled".into();
                return Ok(());
            }
            KeyCode::Down => {
                let n = p.filtered().len();
                if n > 0 {
                    p.selected = (p.selected + 1).min(n - 1);
                }
                return Ok(());
            }
            KeyCode::Up => {
                p.selected = p.selected.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Backspace => {
                p.query.pop();
                p.selected = 0;
                return Ok(());
            }
            KeyCode::Enter => {
                let val = p.filtered().get(p.selected).map(|s| s.to_string());
                let Some(val) = val else {
                    app.status = "no matching status".into();
                    return Ok(());
                };
                commit = Some((p.target, val));
            }
            KeyCode::Char(c) => {
                p.query.push(c);
                p.selected = 0;
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
    if let Some((target, val)) = commit {
        if let Mode::RuleEdit(form) = &mut app.mode {
            form.picker = None;
            match target {
                RulePickerTarget::ActionTransitionTo(i) => {
                    if let Some(jui_core::rules::Action::JiraTransition { to }) =
                        form.rule.actions.get_mut(i)
                    {
                        *to = val.clone();
                    }
                }
            }
            app.status = format!("transition target = \"{val}\"");
        }
    }
    Ok(())
}

fn apply_edit(rule: &mut jui_core::rules::Rule, target: Option<RuleEditTarget>, value: String) {
    use jui_core::rules::{Action, Condition, Trigger};
    let Some(t) = target else { return };
    match t {
        RuleEditTarget::Name => {
            rule.name = value;
        }
        RuleEditTarget::TriggerFilter => match &mut rule.trigger {
            Trigger::TicketStatusChanged { to, .. } => {
                *to = if value.trim().is_empty() {
                    None
                } else {
                    Some(value.trim().to_string())
                };
            }
            Trigger::TicketAssigned { to_me } => {
                let v = value.trim().to_ascii_lowercase();
                *to_me = match v.as_str() {
                    "true" | "yes" | "y" | "1" => Some(true),
                    "false" | "no" | "n" | "0" => Some(false),
                    "" => None,
                    _ => *to_me,
                };
            }
            _ => {}
        },
        RuleEditTarget::ConditionValue(i) => {
            if let Some(c) = rule.conditions.get_mut(i) {
                match c {
                    Condition::ProjectKeyEquals { value: v } => *v = value,
                    Condition::StatusEquals { value: v } => *v = value,
                    Condition::IssueTypeIn { values } => {
                        *values = value
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                    _ => {}
                }
            }
        }
        RuleEditTarget::ActionValue(i) => {
            if let Some(a) = rule.actions.get_mut(i) {
                match a {
                    Action::JiraTransition { to } => *to = value,
                    Action::JiraComment { body } => *body = value,
                    Action::GithubPrComment { body } => *body = value,
                    Action::SetTicketDevQa => {}
                }
            }
        }
    }
}

async fn detail_comments_keys(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> Result<()> {
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if !app.comments.is_empty() {
                app.comment_selected = (app.comment_selected + 1).min(app.comments.len() - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.comment_selected = app.comment_selected.saturating_sub(1);
        }
        KeyCode::Char('c') => {
            if let Some(t) = &app.detail {
                app.mode = Mode::Comment(CommentForm {
                    key: t.key.clone(),
                    body: String::new(),
                    reply_to: None,
                    from_stop_work: false,
                });
            }
        }
        KeyCode::Char('R') => {
            if let (Some(t), Some(c)) = (&app.detail, app.comments.get(app.comment_selected)) {
                app.mode = Mode::Comment(CommentForm {
                    key: t.key.clone(),
                    body: String::new(),
                    reply_to: Some(ReplyContext {
                        parent_author: c.author.clone(),
                        parent_date: c.created.clone(),
                        parent_body: c.body.clone(),
                    }),
                    from_stop_work: false,
                });
            }
        }
        KeyCode::Char('d') => {
            let Some(c) = app.comments.get(app.comment_selected) else {
                return Ok(());
            };
            if !app.comment_is_mine(c) {
                app.status = "can't delete: not your comment".into();
                app.pending_delete = None;
            } else if let Some(id) = c.id.clone() {
                let confirm =
                    matches!(&app.pending_delete, Some(PendingDelete::Comment(p)) if *p == id);
                if confirm {
                    app.pending_delete = None;
                    app.delete_selected_comment().await?;
                } else {
                    app.pending_delete = Some(PendingDelete::Comment(id));
                    app.status =
                        "press 'd' again to delete this comment, or any other key to cancel".into();
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Indices into `app.detail.subtasks` that are currently visible (after the
/// "hide archived" filter). Returned in the original order.
pub fn visible_subtask_indices(app: &App) -> Vec<usize> {
    let Some(t) = &app.detail else {
        return Vec::new();
    };
    if app.show_archived_subtasks {
        return (0..t.subtasks.len()).collect();
    }
    let archived = [
        "resolved",
        "done",
        "closed",
        "archive",
        "archived",
        "won't do",
        "wont do",
        "cancelled",
        "canceled",
    ];
    t.subtasks
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            let st = s.status.as_deref().unwrap_or("").to_ascii_lowercase();
            !archived.iter().any(|a| *a == st)
        })
        .map(|(i, _)| i)
        .collect()
}

fn visible_subtasks_for(app: &App) -> usize {
    visible_subtask_indices(app).len()
}

async fn detail_subtasks_keys(app: &mut App, code: KeyCode) -> Result<()> {
    let n = visible_subtasks_for(app);
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if n > 0 {
                app.subtask_selected = (app.subtask_selected + 1).min(n - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.subtask_selected = app.subtask_selected.saturating_sub(1);
        }
        KeyCode::Char('A') => {
            app.show_archived_subtasks = !app.show_archived_subtasks;
            // Clamp the selection so it stays inside the (possibly smaller) visible range.
            let visible = visible_subtasks_for(app);
            if visible == 0 {
                app.subtask_selected = 0;
            } else if app.subtask_selected >= visible {
                app.subtask_selected = visible - 1;
            }
            app.status = if app.show_archived_subtasks {
                "subtasks: showing archived".into()
            } else {
                "subtasks: hiding archived".into()
            };
        }
        KeyCode::Char('a') | KeyCode::Char('+') | KeyCode::Char('T') => {
            // Add a new subtask of the current ticket.
            if let Some(t) = &app.detail {
                let project_key = t.key.split('-').next().unwrap_or("").to_string();
                app.mode = Mode::Create(CreateForm {
                    project: project_key,
                    issue_type: default_child_type(t).into(),
                    summary: String::new(),
                    description: String::new(),
                    time_estimate: String::new(),
                    priority: String::new(),
                    field: 2,
                    assignee: String::new(),
                    assignee_id: None,
                    assignee_results: vec![],
                    assignee_picker_selected: 0,
                    parent: Some(t.key.clone()),
                    error: None,
                });
            }
        }
        KeyCode::Char('D') => {
            let real_idx = visible_subtask_indices(app)
                .get(app.subtask_selected)
                .copied();
            let sub = real_idx
                .and_then(|i| app.detail.as_ref().and_then(|t| t.subtasks.get(i)))
                .cloned();
            if let Some(s) = sub {
                app.mode = Mode::ArchiveConfirm(ArchiveConfirmForm {
                    key: s.key,
                    summary: s.summary,
                    origin: DeleteOrigin::Subtasks,
                    error: None,
                });
            }
        }
        KeyCode::Enter => {
            // Drill into the selected subtask. Push the current ticket onto the
            // back-stack so Esc returns to the parent rather than all the way to List.
            let real_idx = visible_subtask_indices(app)
                .get(app.subtask_selected)
                .copied();
            let key = real_idx
                .and_then(|i| app.detail.as_ref().and_then(|t| t.subtasks.get(i)))
                .map(|s| s.key.clone());
            if let Some(k) = key {
                if let Some(parent_key) = app.detail.as_ref().map(|t| t.key.clone()) {
                    app.nav_stack.push(NavFrame::Detail {
                        ticket_key: parent_key,
                        focus: app.detail_focus,
                        subtask_selected: app.subtask_selected,
                        comment_selected: app.comment_selected,
                    });
                }
                app.detail_focus = DetailFocus::Info;
                app.open_ticket_by_key(k).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn detail_projects_keys(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if !app.detail_linked_projects.is_empty() {
                app.linked_project_selected =
                    (app.linked_project_selected + 1).min(app.detail_linked_projects.len() - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.linked_project_selected = app.linked_project_selected.saturating_sub(1);
        }
        KeyCode::Char('a') | KeyCode::Char('+') => {
            app.open_ticket_projects().await?;
        }
        KeyCode::Char('y') => {
            // Approve a suggested project.
            let Some(p) = app.detail_linked_projects.get(app.linked_project_selected) else {
                return Ok(());
            };
            if p.state == "no_match" {
                app.status = "nothing to approve — claude couldn't find a match".into();
                return Ok(());
            }
            if p.state == "worktree" {
                app.status = "worktree is already available for start".into();
                return Ok(());
            }
            if p.state != "suggested" {
                app.status = "nothing to approve here (already confirmed)".into();
                return Ok(());
            }
            let Some(t) = &app.detail else { return Ok(()) };
            let req = Request::ConfirmSuggestion {
                ticket_key: t.key.clone(),
                project_path: p.project.path.clone(),
            };
            let mut s = ipc::connect().await?;
            match ipc::send_request(&mut s, &req).await? {
                Response::Ok => {
                    app.status = format!("approved {}", p.project.path.display());
                    app.load_detail().await?;
                }
                Response::Err { message } => app.status = format!("err: {message}"),
                _ => app.status = "unexpected response".into(),
            }
        }
        KeyCode::Char('d') => {
            let Some(p) = app.detail_linked_projects.get(app.linked_project_selected) else {
                return Ok(());
            };
            // Synthetic "no_match" row → dismiss (writes 'rejected' on the synthetic
            // path so it stops surfacing, but the original ticket is still considered
            // "already evaluated" and won't be re-suggested).
            if p.state == "no_match" {
                let Some(t) = &app.detail else { return Ok(()) };
                let req = Request::RejectSuggestion {
                    ticket_key: t.key.clone(),
                    project_path: std::path::PathBuf::from("(none)"),
                };
                let mut s = ipc::connect().await?;
                match ipc::send_request(&mut s, &req).await? {
                    Response::Ok => {
                        app.status = "dismissed".into();
                        app.load_detail().await?;
                    }
                    Response::Err { message } => app.status = format!("err: {message}"),
                    _ => app.status = "unexpected response".into(),
                }
                return Ok(());
            }
            if p.state == "worktree" {
                app.status =
                    "worktree row is detected from git; remove it with git worktree remove".into();
                return Ok(());
            }
            // Suggested → single-press dismiss (no confirm; we're not destroying user data).
            if p.state == "suggested" {
                let Some(t) = &app.detail else { return Ok(()) };
                let req = Request::RejectSuggestion {
                    ticket_key: t.key.clone(),
                    project_path: p.project.path.clone(),
                };
                let mut s = ipc::connect().await?;
                match ipc::send_request(&mut s, &req).await? {
                    Response::Ok => {
                        app.status = format!("dismissed {}", p.project.path.display());
                        app.load_detail().await?;
                    }
                    Response::Err { message } => app.status = format!("err: {message}"),
                    _ => app.status = "unexpected response".into(),
                }
                return Ok(());
            }
            // Confirmed → two-press unlink.
            let path = p.project.path.clone();
            let confirm =
                matches!(&app.pending_delete, Some(PendingDelete::Link(pp)) if *pp == path);
            if confirm {
                app.pending_delete = None;
                if let Some(t) = &app.detail {
                    let key = t.key.clone();
                    let mut s = ipc::connect().await?;
                    let resp = ipc::send_request(
                        &mut s,
                        &Request::UnlinkProject {
                            ticket_key: key,
                            project_path: path.clone(),
                        },
                    )
                    .await?;
                    match resp {
                        Response::Ok => {
                            app.status = format!("unlinked {}", path.display());
                            app.load_detail().await?;
                        }
                        Response::Err { message } => app.status = format!("err: {message}"),
                        _ => app.status = "unexpected response".into(),
                    }
                }
            } else {
                app.pending_delete = Some(PendingDelete::Link(path));
                app.status = "press 'd' again to unlink this project".into();
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Result<()> {
    if matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL) {
        if matches!(&app.mode, Mode::CopilotFixRun(_)) {
            app.kill_copilot_fix_job();
            return Ok(());
        }
        app.should_quit = true;
        return Ok(());
    }
    // Help overlay: toggle on '?'. While open, swallow other keys until the user closes it.
    if app.show_help {
        if matches!(code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')) {
            app.show_help = false;
        }
        return Ok(());
    }
    // Don't open help while typing into a text field — '?' is a legal character there.
    let in_text_input = matches!(
        &app.mode,
        Mode::Edit(_)
            | Mode::Comment(_)
            | Mode::Create(_)
            | Mode::EditTime(_)
            | Mode::EditPriority(_)
            | Mode::Implementation(_)
            | Mode::ProjectsAdd(_)
            | Mode::KanbanFilter(_)
            | Mode::StartWorkPrompt(_)
            | Mode::DevQaPrompt(_)
            | Mode::DevQaResolveConfirm(_)
            | Mode::DevQaCleanupConfirm(_)
            | Mode::ConfluenceSpaces(_)
            | Mode::ConfluencePages(_)
            | Mode::AssignPicker(_)
            | Mode::PrCreate(_)
            | Mode::ActiveStatusConfig(_)
            | Mode::Settings(_)
            | Mode::PrCommentReply(_)
            | Mode::RuleEdit(_)
            | Mode::RuleLog(_)
    );
    if !in_text_input && matches!(code, KeyCode::Char('?')) {
        app.show_help = true;
        return Ok(());
    }
    if matches!(&app.mode, Mode::CopilotFixRun(_)) {
        return copilot_fix_keys(app, code, mods).await;
    }
    // Unified Q: every navigable view's `q` walks back through the nav
    // stack; when empty, the app exits. Esc keeps its per-mode semantics
    // (cancels modals, returns to parent of sub-modes, etc.) — only `q`
    // routes through the stack. Modal forms with text input never see `q`
    // as a back command (they're in the in_text_input set above).
    let q_press = matches!(code, KeyCode::Char('q'));
    let in_navigable = matches!(
        &app.mode,
        Mode::Home(_)
            | Mode::List
            | Mode::Archive
            | Mode::Kanban
            | Mode::Tree(_)
            | Mode::PullRequests(_)
            | Mode::Projects(_)
            | Mode::ConfluenceSpaces(_)
            | Mode::Settings(_)
            | Mode::Rules(_)
            | Mode::RuleLog(_)
            | Mode::RuleEdit(_)
            | Mode::ActiveStatusConfig(_)
    );
    if q_press && in_navigable {
        // ActiveStatusConfig's add-mode shouldn't quit-back — it's typing.
        let block = matches!(
            &app.mode,
            Mode::ActiveStatusConfig(f) if f.adding.is_some()
        );
        if !block {
            app.pop_back_or_quit().await?;
            return Ok(());
        }
    }
    // Esc on a top-level view (other than List) also pops the stack so
    // users who default to Esc don't get stuck. Settings/Rules/RuleEdit/
    // RuleLog handle Esc inside their own dispatch above, so this only
    // covers Archive/Kanban/Tree/Projects/Confluence + Home itself.
    if matches!(code, KeyCode::Esc)
        && matches!(
            &app.mode,
            Mode::Home(_)
                | Mode::Archive
                | Mode::Kanban
                | Mode::Tree(_)
                | Mode::PullRequests(_)
                | Mode::Projects(_)
                | Mode::ConfluenceSpaces(_)
        )
    {
        app.pop_back_or_quit().await?;
        return Ok(());
    }
    // Sub-dispatchers for modes that hold async-heavy state (pickers, IPC
    // round-trips); routed AFTER the unified Q/Esc handler so `q` lands on
    // pop_back_or_quit instead of each pane's bespoke quit logic.
    if matches!(&app.mode, Mode::Settings(_)) {
        return settings_keys(app, code, mods).await;
    }
    if matches!(&app.mode, Mode::Rules(_)) {
        return rules_keys(app, code, mods).await;
    }
    if matches!(&app.mode, Mode::RuleEdit(_)) {
        return rule_edit_keys(app, code, mods).await;
    }
    if matches!(&app.mode, Mode::RuleLog(_)) {
        return rule_log_keys(app, code, mods).await;
    }
    if matches!(&app.mode, Mode::PullRequests(_)) {
        return pull_requests_keys(app, code, mods).await;
    }
    if matches!(&app.mode, Mode::Home(_)) {
        return home_keys(app, code, mods).await;
    }
    match &mut app.mode {
        Mode::List => match code {
            // `q` handled by the unified Q dispatch above.
            KeyCode::Char('/') if app.list_focus == ListFocus::Active => {
                app.ticket_search_active = true;
                app.ticket_search_query.clear();
                app.list_selected = 0;
                app.status = "ticket search: type key/title, Esc clears".into();
            }
            KeyCode::Esc if app.ticket_search_active || !app.ticket_search_query.is_empty() => {
                app.ticket_search_active = false;
                app.ticket_search_query.clear();
                app.list_selected = 0;
                app.status = "ticket search cleared".into();
            }
            KeyCode::Backspace if app.ticket_search_active => {
                app.ticket_search_query.pop();
                app.clamp_list_search_selection();
            }
            KeyCode::Char(c)
                if app.ticket_search_active && !mods.contains(KeyModifiers::CONTROL) =>
            {
                app.ticket_search_query.push(c);
                app.clamp_list_search_selection();
            }
            KeyCode::BackTab => {
                // Shift-Tab cycles focus between the Active list and the
                // Mentioned list at the bottom.
                app.list_focus = match app.list_focus {
                    ListFocus::Active => ListFocus::Mentioned,
                    ListFocus::Mentioned => ListFocus::Active,
                };
            }
            KeyCode::Char('K') => {
                // Toggle visibility of Completed PRs in the bottom section,
                // regardless of which list focus is active. Tree mode also
                // honours the toggle but needs a rebuild to apply (handled
                // elsewhere — Tree mode has its own K handler).
                app.show_completed_prs = !app.show_completed_prs;
                app.status = if app.show_completed_prs {
                    "PRs: showing completed".into()
                } else {
                    "PRs: hiding completed".into()
                };
            }
            KeyCode::Char('j') | KeyCode::Down => match app.list_focus {
                ListFocus::Active => {
                    let n = app.active_search_rows().len();
                    if n > 0 {
                        app.list_selected = (app.list_selected + 1).min(n - 1);
                    }
                }
                ListFocus::Mentioned => {
                    let n = app.reviewing_tickets.len()
                        + app.github_tickets.len()
                        + app.mentioned_tickets.len();
                    if n > 0 {
                        app.mentioned_selected = (app.mentioned_selected + 1).min(n - 1);
                    }
                }
            },
            KeyCode::Char('k') | KeyCode::Up => match app.list_focus {
                ListFocus::Active => {
                    app.list_selected = app.list_selected.saturating_sub(1);
                }
                ListFocus::Mentioned => {
                    app.mentioned_selected = app.mentioned_selected.saturating_sub(1);
                }
            },
            KeyCode::Char('r') => {
                app.refresh().await?;
            }
            KeyCode::Char('o') => {
                app.sort_mode = app.sort_mode.next();
                app.recompute_indexes();
                app.status = format!("sort: {}", app.sort_mode.label());
            }
            KeyCode::Char('s') => {
                app.start_work().await?;
            }
            KeyCode::Char('M') => {
                app.show_all_mine = !app.show_all_mine;
                app.status = if app.show_all_mine {
                    "showing ALL assigned to me · r: refresh".into()
                } else {
                    "showing active work · r: refresh".into()
                };
                app.refresh().await?;
            }
            KeyCode::Tab => {
                // Tab expands subtasks in the Active section; no-op in Mentioned.
                if app.list_focus != ListFocus::Active {
                    return Ok(());
                }
                let rows = app.active_search_rows();
                if let Some((_, t_idx)) = rows.get(app.list_selected).copied() {
                    let key = app.tickets[t_idx].key.clone();
                    let has_children = app.parent_child_counts.get(&key).copied().unwrap_or(0) > 0;
                    if app.expanded_parents.contains(&key) {
                        app.expanded_parents.remove(&key);
                    } else if has_children {
                        app.expanded_parents.insert(key.clone());
                    } else {
                        return Ok(());
                    }
                    app.recompute_indexes();
                    if let Some(new_pos) = app
                        .active_idxs
                        .iter()
                        .position(|i| app.tickets[*i].key == key)
                    {
                        app.list_selected = new_pos;
                    }
                    app.clamp_list_search_selection();
                }
            }
            KeyCode::Char('n') => {
                app.mode = Mode::Create(CreateForm {
                    project: String::new(),
                    issue_type: "Task".into(),
                    summary: String::new(),
                    description: String::new(),
                    time_estimate: String::new(),
                    priority: String::new(),
                    field: 0,
                    assignee: String::new(),
                    assignee_id: None,
                    assignee_results: vec![],
                    assignee_picker_selected: 0,
                    parent: None,
                    error: None,
                });
            }
            KeyCode::Enter => {
                if app.current_ticket().is_some() {
                    app.push_current_view();
                    app.detail_origin = DetailOrigin::List;
                    app.detail_focus = DetailFocus::Info;
                    app.load_detail().await?;
                    app.mode = Mode::Detail;
                }
            }
            _ => {}
        },
        Mode::Archive => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('a') => app.mode = Mode::List,
            KeyCode::Char('j') | KeyCode::Down => {
                if !app.inactive_idxs.is_empty() {
                    app.archive_selected =
                        (app.archive_selected + 1).min(app.inactive_idxs.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.archive_selected = app.archive_selected.saturating_sub(1);
            }
            KeyCode::Char('r') => {
                app.refresh().await?;
            }
            KeyCode::Char('o') => {
                app.sort_mode = app.sort_mode.next();
                app.recompute_indexes();
                app.status = format!("sort: {}", app.sort_mode.label());
            }
            KeyCode::Enter => {
                if app.current_ticket().is_some() {
                    app.detail_origin = DetailOrigin::Archive;
                    app.detail_focus = DetailFocus::Info;
                    app.load_detail().await?;
                    app.mode = Mode::Detail;
                }
            }
            _ => {}
        },
        Mode::Kanban => {
            let cols = app.kanban_columns();
            if cols.is_empty() {
                app.kanban_col = 0;
                app.kanban_expanded_col = None;
                app.kanban_card_per_col.clear();
                app.kanban_minimized.clear();
            } else {
                app.kanban_col = app.kanban_col.min(cols.len() - 1);
                if app
                    .kanban_expanded_col
                    .map(|ci| ci >= cols.len())
                    .unwrap_or(false)
                {
                    app.kanban_expanded_col = None;
                }
                app.kanban_minimized.retain(|ci| *ci < cols.len());
            }
            if app.kanban_card_per_col.len() != cols.len() {
                app.kanban_card_per_col.resize(cols.len(), 0);
            }
            for (ci, (_, idxs)) in cols.iter().enumerate() {
                if !idxs.is_empty() {
                    app.kanban_card_per_col[ci] = app.kanban_card_per_col[ci].min(idxs.len() - 1);
                }
            }
            match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('b') => {
                    app.kanban_expanded_col = None;
                    app.mode = Mode::List;
                }
                KeyCode::Char('r') => {
                    app.refresh().await?;
                }
                // Shift+←/→ (or Shift+H/L) reorder the selected column and persist.
                KeyCode::Left if mods.contains(KeyModifiers::SHIFT) => {
                    app.move_kanban_column(false);
                }
                KeyCode::Right if mods.contains(KeyModifiers::SHIFT) => {
                    app.move_kanban_column(true);
                }
                KeyCode::Char('H') => {
                    app.move_kanban_column(false);
                }
                KeyCode::Char('L') => {
                    app.move_kanban_column(true);
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    app.kanban_col = app.kanban_col.saturating_sub(1);
                    app.kanban_expanded_col = None;
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    if !cols.is_empty() {
                        app.kanban_col = (app.kanban_col + 1).min(cols.len() - 1);
                    }
                    app.kanban_expanded_col = None;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(col) = cols.get(app.kanban_col) {
                        let n = col.1.len();
                        if n > 0 {
                            let cur = app.kanban_card_per_col[app.kanban_col];
                            app.kanban_card_per_col[app.kanban_col] = (cur + 1).min(n - 1);
                        }
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(_col) = cols.get(app.kanban_col) {
                        let cur = app.kanban_card_per_col[app.kanban_col];
                        app.kanban_card_per_col[app.kanban_col] = cur.saturating_sub(1);
                    }
                }
                KeyCode::Char('u') => {
                    let mut form = KanbanFilterForm::new();
                    let mut s = ipc::connect().await?;
                    if let Ok(ipc::Response::Users { items, from_cache }) = ipc::send_request(
                        &mut s,
                        &ipc::Request::SearchUsers {
                            query: String::new(),
                        },
                    )
                    .await
                    {
                        form.results = items
                            .into_iter()
                            .map(|u| (u.display_name, u.account_id))
                            .collect();
                        form.from_cache = from_cache;
                    }
                    let mut s2 = ipc::connect().await?;
                    if let Ok(ipc::Response::Teams { items }) =
                        ipc::send_request(&mut s2, &ipc::Request::ListTeams).await
                    {
                        form.teams = items;
                    }
                    app.mode = Mode::KanbanFilter(form);
                }
                KeyCode::Enter => {
                    let n = app.kanban_columns().len();
                    if app.kanban_minimized.contains(&app.kanban_col) {
                        // Expand minimized column.
                        app.kanban_minimized.remove(&app.kanban_col);
                    } else if app.current_ticket().is_some() {
                        app.detail_origin = DetailOrigin::Kanban;
                        app.detail_focus = DetailFocus::Info;
                        app.load_detail().await?;
                        app.mode = Mode::Detail;
                    }
                    let _ = n;
                }
                KeyCode::Char('m') => {
                    let n = app.kanban_columns().len();
                    if n > 0 {
                        let ci = app.kanban_col;
                        if app.kanban_minimized.contains(&ci) {
                            app.kanban_minimized.remove(&ci);
                        } else {
                            app.kanban_minimized.insert(ci);
                        }
                    }
                }
                KeyCode::Char('e') => {
                    let ci = app.kanban_col;
                    if app.kanban_expanded_col == Some(ci) {
                        app.kanban_expanded_col = None;
                    } else {
                        app.kanban_expanded_col = Some(ci);
                    }
                }
                _ => {}
            }
        }
        Mode::KanbanFilter(ref form) => {
            let n_teams = form.teams.len();
            let total_rows = form.total_rows();
            let selected = form.selected;
            let saving = form.save_name.is_some();

            // --- Save-name input active ---
            if saving {
                match code {
                    KeyCode::Esc => {
                        if let Mode::KanbanFilter(ref mut form) = app.mode {
                            form.save_name = None;
                        }
                    }
                    KeyCode::Enter => {
                        let (save_name, members) = if let Mode::KanbanFilter(ref form) = app.mode {
                            (
                                form.save_name.clone().unwrap_or_default(),
                                app.kanban_assignee_filter
                                    .iter()
                                    .cloned()
                                    .collect::<Vec<_>>(),
                            )
                        } else {
                            (String::new(), vec![])
                        };
                        if !save_name.trim().is_empty() && !members.is_empty() {
                            let mut s = ipc::connect().await?;
                            let _ = ipc::send_request(
                                &mut s,
                                &ipc::Request::SaveTeam {
                                    name: save_name.trim().to_string(),
                                    members,
                                },
                            )
                            .await;
                            // Refresh teams list.
                            let mut s2 = ipc::connect().await?;
                            if let Ok(ipc::Response::Teams { items }) =
                                ipc::send_request(&mut s2, &ipc::Request::ListTeams).await
                            {
                                if let Mode::KanbanFilter(ref mut form) = app.mode {
                                    form.teams = items;
                                    form.save_name = None;
                                }
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if let Mode::KanbanFilter(ref mut form) = app.mode {
                            if let Some(ref mut name) = form.save_name {
                                name.pop();
                            }
                        }
                    }
                    KeyCode::Char(ch) if !mods.contains(KeyModifiers::CONTROL) => {
                        if let Mode::KanbanFilter(ref mut form) = app.mode {
                            if let Some(ref mut name) = form.save_name {
                                name.push(ch);
                            }
                        }
                    }
                    _ => {}
                }
                return Ok(());
            }

            // --- Normal filter navigation ---
            match code {
                KeyCode::Esc => {
                    app.mode = Mode::Kanban;
                }
                KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => {
                    app.kanban_assignee_filter.clear();
                    app.kanban_extra.clear();
                    app.mode = Mode::Kanban;
                }
                KeyCode::Char('s')
                    if mods.contains(KeyModifiers::CONTROL)
                        && !app.kanban_assignee_filter.is_empty() =>
                {
                    if let Mode::KanbanFilter(ref mut form) = app.mode {
                        form.save_name = Some(String::new());
                    }
                }
                KeyCode::Char('d') if mods.contains(KeyModifiers::CONTROL) => {
                    // Delete team if cursor on a team row.
                    if selected < n_teams {
                        let team_name = if let Mode::KanbanFilter(ref form) = app.mode {
                            form.teams.get(form.selected).map(|t| t.name.clone())
                        } else {
                            None
                        };
                        if let Some(name) = team_name {
                            let mut s = ipc::connect().await?;
                            let _ =
                                ipc::send_request(&mut s, &ipc::Request::DeleteTeam { name }).await;
                            let mut s2 = ipc::connect().await?;
                            if let Ok(ipc::Response::Teams { items }) =
                                ipc::send_request(&mut s2, &ipc::Request::ListTeams).await
                            {
                                if let Mode::KanbanFilter(ref mut form) = app.mode {
                                    form.teams = items;
                                    form.selected =
                                        form.selected.min(form.total_rows().saturating_sub(1));
                                }
                            }
                        }
                    }
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if selected < n_teams {
                        // Apply team — replace filter with team members.
                        let members = if let Mode::KanbanFilter(ref form) = app.mode {
                            form.teams.get(form.selected).map(|t| t.members.clone())
                        } else {
                            None
                        };
                        if let Some(members) = members {
                            app.kanban_assignee_filter = members.into_iter().collect();
                        }
                    } else {
                        // Toggle individual user.
                        let user_idx = selected - n_teams;
                        let name = if let Mode::KanbanFilter(ref form) = app.mode {
                            form.results.get(user_idx).map(|(n, _)| n.clone())
                        } else {
                            None
                        };
                        if let Some(name) = name {
                            if app.kanban_assignee_filter.contains(&name) {
                                app.kanban_assignee_filter.remove(&name);
                            } else {
                                app.kanban_assignee_filter.insert(name);
                            }
                        }
                    }
                    app.refresh_kanban_for_users().await?;
                    if matches!(code, KeyCode::Enter) {
                        app.mode = Mode::Kanban;
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Mode::KanbanFilter(ref mut form) = app.mode {
                        if total_rows > 0 {
                            form.selected = (selected + 1).min(total_rows - 1);
                        }
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Mode::KanbanFilter(ref mut form) = app.mode {
                        form.selected = selected.saturating_sub(1);
                    }
                }
                KeyCode::Backspace => {
                    if let Mode::KanbanFilter(ref mut form) = app.mode {
                        form.query.pop();
                        let q = form.query.clone();
                        let mut s = ipc::connect().await?;
                        if let Ok(ipc::Response::Users { items, from_cache }) =
                            ipc::send_request(&mut s, &ipc::Request::SearchUsers { query: q }).await
                        {
                            form.results = items
                                .into_iter()
                                .map(|u| (u.display_name, u.account_id))
                                .collect();
                            form.from_cache = from_cache;
                        }
                    }
                }
                KeyCode::Char(ch) if !mods.contains(KeyModifiers::CONTROL) => {
                    if let Mode::KanbanFilter(ref mut form) = app.mode {
                        form.query.push(ch);
                        let q = form.query.clone();
                        let mut s = ipc::connect().await?;
                        if let Ok(ipc::Response::Users { items, from_cache }) =
                            ipc::send_request(&mut s, &ipc::Request::SearchUsers { query: q }).await
                        {
                            form.results = items
                                .into_iter()
                                .map(|u| (u.display_name, u.account_id))
                                .collect();
                            form.from_cache = from_cache;
                        }
                    }
                }
                _ => {}
            }
        }
        Mode::Detail => {
            // Any key other than 'd' clears a pending delete confirmation.
            if !matches!(code, KeyCode::Char('d')) {
                app.pending_delete = None;
            }
            // Tab cycles focus regardless of which pane is active. Skip panes
            // that aren't visible for the current ticket: Subtasks pane is
            // hidden when the ticket is itself a sub-task; PrComments pane is
            // hidden when there are no GitHub PR comments cached for this
            // ticket.
            if matches!(code, KeyCode::Tab | KeyCode::BackTab) {
                let is_subtask = app
                    .detail
                    .as_ref()
                    .and_then(|t| t.issue_type.as_deref())
                    .map(|x| {
                        x.eq_ignore_ascii_case("sub-task") || x.eq_ignore_ascii_case("subtask")
                    })
                    .unwrap_or(false);
                let pr_visible = !app.pr_comments.is_empty();
                let backwards = matches!(code, KeyCode::BackTab);
                for _ in 0..6 {
                    app.detail_focus = if backwards {
                        app.detail_focus.prev()
                    } else {
                        app.detail_focus.next()
                    };
                    let ok = match app.detail_focus {
                        DetailFocus::Subtasks if is_subtask => false,
                        DetailFocus::PrComments if !pr_visible => false,
                        _ => true,
                    };
                    if ok {
                        break;
                    }
                }
                return Ok(());
            }
            // Esc / q: pop the back-stack if we have one, otherwise fall back to
            // the original origin. This keeps Tree → Detail and Detail → Subtask
            // navigation symmetric.
            if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                if let Some(frame) = app.nav_stack.pop() {
                    match frame {
                        NavFrame::Tree(form) => {
                            app.mode = Mode::Tree(*form);
                        }
                        NavFrame::Detail {
                            ticket_key,
                            focus,
                            subtask_selected,
                            comment_selected,
                        } => {
                            app.open_ticket_by_key(ticket_key).await?;
                            // Restore the pane focus + cursor positions the user had
                            // before drilling into the subtask.
                            app.detail_focus = focus;
                            app.subtask_selected = subtask_selected;
                            app.comment_selected = comment_selected;
                            app.mode = Mode::Detail;
                        }
                        NavFrame::List => app.mode = Mode::List,
                        NavFrame::Archive => app.mode = Mode::Archive,
                        NavFrame::Kanban => app.mode = Mode::Kanban,
                        NavFrame::Home => {
                            app.mode = Mode::Home(HomeForm {
                                items: Vec::new(),
                                selected: 0,
                                loading: true,
                                error: None,
                                menu_selected: 0,
                                focus: HomeFocus::Menu,
                            });
                            let _ = app.load_home_activity().await;
                        }
                        NavFrame::Settings => app.open_settings(),
                        NavFrame::Rules => app.open_rules(),
                        NavFrame::RuleLog => app.open_rule_log().await?,
                        NavFrame::RuleEdit(_) => app.open_rules(),
                        NavFrame::Projects => app.open_projects().await?,
                        NavFrame::PullRequests => app.open_pull_requests().await?,
                        NavFrame::Confluence => app.open_confluence_spaces().await?,
                        NavFrame::ActiveStatusConfig => app.open_active_status_config(),
                    }
                } else {
                    app.mode = match app.detail_origin {
                        DetailOrigin::List => Mode::List,
                        DetailOrigin::Archive => Mode::Archive,
                        DetailOrigin::Kanban => Mode::Kanban,
                    };
                }
                return Ok(());
            }
            // Global ticket-level shortcuts — fire regardless of which pane is
            // focused, since these act on the ticket itself (not a pane selection).
            // Keys that DO depend on selection (j/k, d, a) stay pane-scoped below.
            match code {
                KeyCode::Char('C') => {
                    app.open_implementation().await?;
                    return Ok(());
                }
                KeyCode::Char('e') => {
                    if let Some(t) = &app.detail {
                        let desc = t.description.clone().unwrap_or_default();
                        let s_cur = t.summary.len();
                        let d_cur = desc.len();
                        app.mode = Mode::Edit(EditForm {
                            key: t.key.clone(),
                            summary: t.summary.clone(),
                            description: desc.clone(),
                            original_summary: t.summary.clone(),
                            original_description: desc,
                            field: 0,
                            suggestion: None,
                            summary_cursor: s_cur,
                            description_cursor: d_cur,
                        });
                    }
                    return Ok(());
                }
                KeyCode::Char('t') => {
                    if let Some(t) = &app.detail {
                        let key = t.key.clone();
                        app.open_transition(key).await?;
                    }
                    return Ok(());
                }
                KeyCode::Char('w') => {
                    if let Some(t) = &app.detail {
                        app.mode = Mode::EditTime(EditTimeForm {
                            key: t.key.clone(),
                            original_estimate: String::new(),
                            log_work: String::new(),
                            field: 0,
                        });
                    }
                    return Ok(());
                }
                KeyCode::Char('i') => {
                    app.open_priority_picker().await?;
                    return Ok(());
                }
                KeyCode::Char('O') => {
                    app.open_ticket_options().await?;
                    return Ok(());
                }
                KeyCode::Char('L') => {
                    app.open_ticket_projects().await?;
                    return Ok(());
                }
                KeyCode::Char('s') => {
                    app.start_work().await?;
                    return Ok(());
                }
                KeyCode::Char('T') => {
                    if let Some(t) = &app.detail {
                        let pt = t.issue_type.as_deref().unwrap_or("");
                        if pt.eq_ignore_ascii_case("sub-task") || pt.eq_ignore_ascii_case("subtask")
                        {
                            app.status = "can't add a child under a sub-task".into();
                            return Ok(());
                        }
                        let project_key = t.key.split('-').next().unwrap_or("").to_string();
                        app.mode = Mode::Create(CreateForm {
                            project: project_key,
                            issue_type: default_child_type(t).into(),
                            summary: String::new(),
                            description: String::new(),
                            time_estimate: String::new(),
                            priority: String::new(),
                            field: 2,
                            assignee: String::new(),
                            assignee_id: None,
                            assignee_results: vec![],
                            assignee_picker_selected: 0,
                            parent: Some(t.key.clone()),
                            error: None,
                        });
                    }
                    return Ok(());
                }
                KeyCode::Char('@') => {
                    app.open_assign_picker(AssignPurpose::Assignee).await?;
                    return Ok(());
                }
                KeyCode::Char('P') => {
                    // Two contexts, mutually exclusive:
                    //   • DevQA started + has PR → resolve DevQA (pass).
                    //   • no PR yet            → open a PR.
                    if ticket_devqa_in_progress(app) && ticket_has_pr(app) {
                        let key = match &app.detail {
                            Some(t) => t.key.clone(),
                            None => return Ok(()),
                        };
                        if let Some(pr_url) = app.current_pr_url() {
                            app.mode = Mode::DevQaResolveConfirm(DevQaResolveForm {
                                ticket_key: key,
                                pr_url,
                                error: None,
                            });
                        } else {
                            app.status = "no PR url to resolve DevQA".into();
                        }
                        return Ok(());
                    }
                    if ticket_has_pr(app) {
                        app.status = "ticket already has a PR".into();
                        return Ok(());
                    }
                    app.open_pr_create().await?;
                    return Ok(());
                }
                KeyCode::Char('K') => {
                    // Cycle the user-managed PR review state. Awaiting →
                    // Reviewing → Completed → Awaiting (lets the user un-mark).
                    let key = match &app.detail {
                        Some(t) => t.key.clone(),
                        None => return Ok(()),
                    };
                    let next = match app.pr_state(&key) {
                        PrUserState::Awaiting => PrUserState::Reviewing,
                        PrUserState::Reviewing => PrUserState::Completed,
                        PrUserState::Completed => PrUserState::Awaiting,
                    };
                    app.set_pr_state(&key, next).await?;
                    app.status = format!(
                        "{key} review marker: {}",
                        match next {
                            PrUserState::Awaiting => "To Review",
                            PrUserState::Reviewing => "Reviewing",
                            PrUserState::Completed => "Done",
                        }
                    );
                    return Ok(());
                }
                KeyCode::Char('Q') => {
                    // Find a transition whose target status contains "dev qa in
                    // progress" (case-insensitive). Site-specific workflows
                    // prefix the status with a team name (e.g. "Firmware Dev
                    // QA In Progress") and the transition itself may just be
                    // called "Next", so matching on the destination is more
                    // reliable than the transition name.
                    let key = match &app.detail {
                        Some(t) => t.key.clone(),
                        None => return Ok(()),
                    };
                    let mut s = ipc::connect().await?;
                    let resp =
                        ipc::send_request(&mut s, &Request::ListTransitions { key: key.clone() })
                            .await?;
                    let target = if let Response::Transitions { items } = resp {
                        let needle = "dev qa in progress";
                        items
                            .iter()
                            .find(|tr| {
                                tr.to_status
                                    .as_deref()
                                    .map(|s| s.to_ascii_lowercase().contains(needle))
                                    .unwrap_or(false)
                                    || tr.name.to_ascii_lowercase().contains(needle)
                            })
                            .cloned()
                    } else {
                        None
                    };
                    let Some(tr) = target else {
                        // No Dev QA transition available — the ticket was very
                        // likely already moved past "begin DevQA". Don't dead-end:
                        //   • if a worktree already exists on disk, re-open Claude
                        //     in it (resume the saved session, or start a fresh
                        //     one) — works even when the PR/session links are gone;
                        //   • else if we still know the PR, prompt to create one;
                        //   • else explain what's missing.
                        let mut s = ipc::connect().await?;
                        let existing_wt = match ipc::send_request(
                            &mut s,
                            &Request::FindDevQaWorktree {
                                ticket_key: key.clone(),
                            },
                        )
                        .await?
                        {
                            Response::DevQaWorktree { path, .. } => Some(path),
                            _ => None,
                        };
                        if let Some(path) = existing_wt {
                            if let Err(e) = app.reopen_devqa_worktree(&key, path).await {
                                app.status = format!("re-open DevQA err: {e:#}");
                            }
                        } else if let Some(pr_url) = app.current_pr_url() {
                            app.mode = Mode::DevQaPrompt(DevQaPromptForm {
                                ticket_key: key.clone(),
                                pr_url,
                                use_worktree: true,
                                error: None,
                            });
                        } else {
                            app.status = format!(
                                "no 'Dev QA In Progress' transition and no worktree, \
                                 session, or PR for {key}"
                            );
                        }
                        return Ok(());
                    };
                    let to_label = tr.to_status.clone().unwrap_or_else(|| tr.name.clone());
                    let mut s = ipc::connect().await?;
                    let resp = ipc::send_request(
                        &mut s,
                        &Request::Transition {
                            key: key.clone(),
                            to: tr.name.clone(),
                        },
                    )
                    .await?;
                    if let Response::Err { message } = resp {
                        app.status = format!("transition err: {message}");
                        return Ok(());
                    }
                    app.status = format!("{key} → {to_label}");
                    // Mark the PR as actively under review.
                    let _ = app.set_pr_state(&key, PrUserState::Reviewing).await;
                    app.load_detail().await?;
                    // If there's a PR, prompt for how to land its branch (worktree
                    // vs. branch-in-repo) before launching Claude. When there's no
                    // associated PR (e.g. a ticket that landed via @-mention), the
                    // transition still stands; say so instead of doing nothing.
                    if let Some(pr_url) = app.current_pr_url() {
                        app.mode = Mode::DevQaPrompt(DevQaPromptForm {
                            ticket_key: key.clone(),
                            pr_url,
                            use_worktree: true,
                            error: None,
                        });
                    } else {
                        app.status = format!("{key} → {to_label} · no PR to DevQA");
                    }
                    return Ok(());
                }
                // Reviewer picker — but only when focus isn't on the PR
                // Comments pane, where `R` resolves the selected thread.
                KeyCode::Char('R') if app.detail_focus != DetailFocus::PrComments => {
                    app.open_assign_picker(AssignPurpose::Reviewer).await?;
                    return Ok(());
                }
                KeyCode::Char('Y') => {
                    app.open_assign_picker(AssignPurpose::DevQa).await?;
                    return Ok(());
                }
                KeyCode::Char('D') => {
                    if let Some(t) = &app.detail {
                        app.mode = Mode::ArchiveConfirm(ArchiveConfirmForm {
                            key: t.key.clone(),
                            summary: t.summary.clone(),
                            origin: DeleteOrigin::DetailInfo,
                            error: None,
                        });
                    }
                    return Ok(());
                }
                _ => {}
            }
            // Pane-scoped keys.
            match app.detail_focus {
                DetailFocus::Projects => {
                    detail_projects_keys(app, code).await?;
                    return Ok(());
                }
                DetailFocus::Subtasks => {
                    detail_subtasks_keys(app, code).await?;
                    return Ok(());
                }
                DetailFocus::Comments => {
                    detail_comments_keys(app, code, mods).await?;
                    return Ok(());
                }
                DetailFocus::PrComments => {
                    let visible = app.visible_pr_comments();
                    match code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            if !visible.is_empty() {
                                app.pr_comment_selected =
                                    (app.pr_comment_selected + 1).min(visible.len() - 1);
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            app.pr_comment_selected = app.pr_comment_selected.saturating_sub(1);
                        }
                        KeyCode::Char('H') => {
                            app.show_resolved_pr_comments = !app.show_resolved_pr_comments;
                            // Clamp selection — the visible-list length just
                            // changed under our feet.
                            let new_visible = app.visible_pr_comments();
                            if !new_visible.is_empty() {
                                app.pr_comment_selected =
                                    app.pr_comment_selected.min(new_visible.len() - 1);
                            } else {
                                app.pr_comment_selected = 0;
                            }
                            app.status = if app.show_resolved_pr_comments {
                                "showing resolved threads".into()
                            } else {
                                "hiding resolved threads".into()
                            };
                        }
                        KeyCode::Char('c') => {
                            app.chat_about_pr_comment().await?;
                        }
                        KeyCode::Char('r') => {
                            app.open_pr_comment_reply();
                        }
                        KeyCode::Char('R') => {
                            app.resolve_selected_pr_comment().await?;
                        }
                        _ => {}
                    }
                    return Ok(());
                }
                DetailFocus::Info => {}
            }
            // Info-pane / global keys (the actions on the ticket itself).
            match code {
                KeyCode::Char('e') => {
                    if let Some(t) = &app.detail {
                        let desc = t.description.clone().unwrap_or_default();
                        let s_cur = t.summary.len();
                        let d_cur = desc.len();
                        app.mode = Mode::Edit(EditForm {
                            key: t.key.clone(),
                            summary: t.summary.clone(),
                            description: desc.clone(),
                            original_summary: t.summary.clone(),
                            original_description: desc,
                            field: 0,
                            suggestion: None,
                            summary_cursor: s_cur,
                            description_cursor: d_cur,
                        });
                    }
                }
                KeyCode::Char('t') => {
                    if let Some(t) = &app.detail {
                        let key = t.key.clone();
                        app.open_transition(key).await?;
                    }
                }
                KeyCode::Char('s') => {
                    app.start_work().await?;
                }
                KeyCode::Char('w') => {
                    if let Some(t) = &app.detail {
                        app.mode = Mode::EditTime(EditTimeForm {
                            key: t.key.clone(),
                            original_estimate: String::new(),
                            log_work: String::new(),
                            field: 0,
                        });
                    }
                }
                KeyCode::Char('L') => {
                    app.open_ticket_projects().await?;
                }
                KeyCode::Char('i') => {
                    app.open_priority_picker().await?;
                }
                KeyCode::Char('C') => {
                    app.open_implementation().await?;
                }
                KeyCode::Char('T') => {
                    if let Some(t) = &app.detail {
                        let project_key = t.key.split('-').next().unwrap_or("").to_string();
                        app.mode = Mode::Create(CreateForm {
                            project: project_key,
                            issue_type: default_child_type(t).into(),
                            summary: String::new(),
                            description: String::new(),
                            time_estimate: String::new(),
                            priority: String::new(),
                            // Project + type are prefilled; jump straight to the summary field.
                            field: 2,
                            assignee: String::new(),
                            assignee_id: None,
                            assignee_results: vec![],
                            assignee_picker_selected: 0,
                            parent: Some(t.key.clone()),
                            error: None,
                        });
                    }
                }
                KeyCode::Char('c') => {
                    if let Some(t) = &app.detail {
                        app.mode = Mode::Comment(CommentForm {
                            key: t.key.clone(),
                            body: String::new(),
                            reply_to: None,
                            from_stop_work: false,
                        });
                    }
                }
                _ => {}
            }
        }
        Mode::Create(form) => {
            const ASSIGNEE_FIELD: u8 = CreateForm::FIELD_COUNT - 1;
            match code {
                KeyCode::Esc => app.mode = Mode::List,
                KeyCode::Tab => {
                    form.field = (form.field + 1) % CreateForm::FIELD_COUNT;
                    if let Mode::Create(f) = &mut app.mode {
                        f.assignee_picker_selected = 0;
                    }
                }
                KeyCode::BackTab => {
                    form.field = if form.field == 0 {
                        CreateForm::FIELD_COUNT - 1
                    } else {
                        form.field - 1
                    };
                    if let Mode::Create(f) = &mut app.mode {
                        f.assignee_picker_selected = 0;
                    }
                }
                KeyCode::F(5) => {
                    app.submit_create().await?;
                }
                KeyCode::Char(c)
                    if matches!(c, 's' | 'S') && mods.contains(KeyModifiers::CONTROL) =>
                {
                    app.submit_create().await?;
                }
                KeyCode::Enter if mods.contains(KeyModifiers::CONTROL) => {
                    app.submit_create().await?;
                }
                // Up/Down on assignee field navigate picker dropdown.
                KeyCode::Up if form.field == ASSIGNEE_FIELD => {
                    form.assignee_picker_selected = form.assignee_picker_selected.saturating_sub(1);
                }
                KeyCode::Down if form.field == ASSIGNEE_FIELD => {
                    if !form.assignee_results.is_empty() {
                        form.assignee_picker_selected = (form.assignee_picker_selected + 1)
                            .min(form.assignee_results.len() - 1);
                    }
                }
                KeyCode::Enter
                    if form.field == ASSIGNEE_FIELD && !form.assignee_results.is_empty() =>
                {
                    if let Some((name, id)) = form
                        .assignee_results
                        .get(form.assignee_picker_selected)
                        .cloned()
                    {
                        form.assignee = name;
                        form.assignee_id = Some(id);
                        form.assignee_results.clear();
                    }
                }
                KeyCode::Enter => {
                    if form.field == ASSIGNEE_FIELD {
                        app.submit_create().await?;
                    } else {
                        form.field += 1;
                    }
                }
                KeyCode::Backspace => {
                    form.field_mut().pop();
                    if form.field == ASSIGNEE_FIELD {
                        let q = form.assignee.clone();
                        if q.len() >= 2 {
                            refresh_assignee_picker(app, &q).await?;
                        } else if let Mode::Create(f) = &mut app.mode {
                            f.assignee_results.clear();
                        }
                    }
                }
                KeyCode::Char(c) => {
                    form.field_mut().push(c);
                    if form.field == ASSIGNEE_FIELD {
                        let q = form.assignee.clone();
                        if q.len() >= 2 {
                            refresh_assignee_picker(app, &q).await?;
                        }
                    }
                }
                _ => {}
            }
        }
        Mode::Edit(form) => {
            // When a Claude suggestion is pending, intercept y/n first.
            if form.suggestion.is_some() {
                match code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        if let Some(s) = form.suggestion.take() {
                            form.description = s;
                            form.description_cursor = form.description.len();
                            form.field = 1;
                        }
                        app.status = "accepted claude rewrite".into();
                        return Ok(());
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        form.suggestion = None;
                        app.status = "rejected claude rewrite".into();
                        return Ok(());
                    }
                    _ => {
                        // Any other key: drop the suggestion and fall through so normal
                        // editing isn't blocked.
                        form.suggestion = None;
                    }
                }
            }
            let is_desc = form.field == 1;
            let (cur, cursor): (&mut String, &mut usize) = if is_desc {
                (&mut form.description, &mut form.description_cursor)
            } else {
                (&mut form.summary, &mut form.summary_cursor)
            };
            // Clamp the cursor in case the buffer shrank under us (paranoia
            // — accepted-suggestion path already resets, but cheap).
            if *cursor > cur.len() {
                *cursor = cur.len();
            }
            match code {
                KeyCode::Esc => app.mode = Mode::Detail,
                KeyCode::Tab | KeyCode::BackTab => {
                    form.field = if form.field == 0 { 1 } else { 0 };
                }
                KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => {
                    app.submit_edit().await?
                }
                KeyCode::Char('e') if mods.contains(KeyModifiers::CONTROL) => {
                    app.edit_ticket_in_editor().await?
                }
                KeyCode::F(5) => app.submit_edit().await?,
                KeyCode::Enter if mods.contains(KeyModifiers::CONTROL) => app.submit_edit().await?,
                // Ctrl+R or F6: ask Claude to tighten the description (works
                // from either field). F6 is the unambiguous fallback for
                // terminals that swallow Ctrl+R.
                KeyCode::Char('r') if mods.contains(KeyModifiers::CONTROL) => {
                    app.improve_edit_description().await?;
                }
                KeyCode::F(6) => {
                    app.improve_edit_description().await?;
                }
                // Cursor navigation.
                KeyCode::Left => {
                    *cursor = edit_left(cur, *cursor);
                }
                KeyCode::Right => {
                    *cursor = edit_right(cur, *cursor);
                }
                KeyCode::Up => {
                    if is_desc {
                        *cursor = edit_up(cur, *cursor);
                    }
                }
                KeyCode::Down => {
                    if is_desc {
                        *cursor = edit_down(cur, *cursor);
                    }
                }
                KeyCode::Home => {
                    *cursor = edit_line_start(cur, *cursor);
                }
                KeyCode::End => {
                    *cursor = edit_line_end(cur, *cursor);
                }
                // Enter in summary submits (single-line); in description inserts newline.
                KeyCode::Enter => {
                    if !is_desc {
                        app.submit_edit().await?;
                    } else {
                        cur.insert(*cursor, '\n');
                        *cursor += 1;
                    }
                }
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        let prev = edit_left(cur, *cursor);
                        cur.replace_range(prev..*cursor, "");
                        *cursor = prev;
                    }
                }
                KeyCode::Delete => {
                    if *cursor < cur.len() {
                        let nxt = edit_right(cur, *cursor);
                        cur.replace_range(*cursor..nxt, "");
                    }
                }
                KeyCode::Char(c) => {
                    cur.insert(*cursor, c);
                    *cursor += c.len_utf8();
                }
                _ => {}
            }
        }
        Mode::Comment(form) => match code {
            KeyCode::Esc => {
                // On stop-work, Esc still stops the work — it just skips the
                // optional comment. Normal comment Esc bails to Detail.
                if form.from_stop_work {
                    let key = form.key.clone();
                    app.submit_stop_work_no_comment(key).await?;
                } else {
                    app.mode = Mode::Detail;
                }
            }
            KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => {
                app.submit_comment().await?
            }
            KeyCode::Enter => form.body.push('\n'),
            KeyCode::Backspace => {
                form.body.pop();
            }
            KeyCode::Char(c) => form.body.push(c),
            _ => {}
        },
        Mode::TicketProjects(form) => match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.mode = Mode::Detail;
                app.load_detail().await?;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.items.is_empty() {
                    form.selected = (form.selected + 1).min(form.items.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected = form.selected.saturating_sub(1);
            }
            KeyCode::Char(' ') | KeyCode::Enter => app.toggle_ticket_project().await?,
            _ => {}
        },
        Mode::Projects(form) => {
            if !matches!(code, KeyCode::Char('d')) {
                form.pending_remove = None;
            }
            match code {
                KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::List,
                KeyCode::Char('j') | KeyCode::Down => {
                    if !form.items.is_empty() {
                        form.selected = (form.selected + 1).min(form.items.len() - 1);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    form.selected = form.selected.saturating_sub(1);
                }
                KeyCode::Char('a') => {
                    app.open_projects_add().await?;
                }
                KeyCode::Char('r') => {
                    app.open_projects().await?;
                }
                KeyCode::Char('d') => {
                    if let Some(p) = form.items.get(form.selected) {
                        let path = p.path.clone();
                        if form.pending_remove.as_ref() == Some(&path) {
                            form.pending_remove = None;
                            app.remove_selected_project().await?;
                        } else {
                            form.pending_remove = Some(path);
                            app.status = "press 'd' again to remove this project".into();
                        }
                    }
                }
                _ => {}
            }
        }
        Mode::ProjectsAdd(form) => match code {
            KeyCode::Esc => {
                app.open_projects().await?;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let n = form.filtered().len();
                if n > 0 {
                    form.selected = (form.selected + 1).min(n - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected = form.selected.saturating_sub(1);
            }
            KeyCode::Enter => app.submit_add_project().await?,
            KeyCode::Backspace => {
                form.query.pop();
                form.selected = 0;
            }
            KeyCode::Char(c) => {
                form.query.push(c);
                form.selected = 0;
            }
            _ => {}
        },
        Mode::Implementation(form) => match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.mode = Mode::Detail;
                app.load_detail().await?;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                form.scroll = form.scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.scroll = form.scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                form.scroll = form.scroll.saturating_add(10);
            }
            KeyCode::PageUp => {
                form.scroll = form.scroll.saturating_sub(10);
            }
            KeyCode::Char('g') => {
                form.scroll = 0;
            }
            KeyCode::Char('s') => {
                app.save_implementation_to_file().await?;
            }
            KeyCode::Char('o') => {
                app.launch_claude_in_tmux().await?;
            }
            KeyCode::Char('r') => {
                app.reload_implementation().await?;
            }
            KeyCode::Char('R') => {
                app.regenerate_implementation().await?;
            }
            _ => {}
        },
        Mode::StartWorkPrompt(form) => {
            // Cycle order: 0 (location) → 1 (branch) → 2 (time, if needed) →
            // 3 (priority, if needed) → 4 (plan-mode toggle) → 5 (extra-shell
            // toggle) → 0
            let visible: Vec<u8> = {
                let mut v = vec![0u8, 1u8];
                if form.need_time {
                    v.push(2);
                }
                if form.need_priority {
                    v.push(3);
                }
                v.push(4);
                v.push(5);
                v
            };
            let cycle = |cur: u8, forward: bool| -> u8 {
                let i = visible.iter().position(|&f| f == cur).unwrap_or(0);
                let n = visible.len();
                let next = if forward {
                    (i + 1) % n
                } else {
                    (i + n - 1) % n
                };
                visible[next]
            };
            match code {
                KeyCode::Esc => app.mode = Mode::Detail,
                KeyCode::Tab => {
                    form.field = cycle(form.field, true);
                }
                KeyCode::BackTab => {
                    form.field = cycle(form.field, false);
                }
                KeyCode::F(5) => {
                    app.submit_start_work_prompt().await?;
                }
                KeyCode::Char(c)
                    if matches!(c, 's' | 'S') && mods.contains(KeyModifiers::CONTROL) =>
                {
                    app.submit_start_work_prompt().await?;
                }
                KeyCode::Enter => {
                    app.submit_start_work_prompt().await?;
                }
                // Location field — arrows or space toggle worktree ↔ branch-in-repo.
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 0 => {
                    form.location = match form.location {
                        jui_core::scm::WorkLocation::Worktree => {
                            jui_core::scm::WorkLocation::BranchInRepo
                        }
                        jui_core::scm::WorkLocation::BranchInRepo => {
                            jui_core::scm::WorkLocation::Worktree
                        }
                    };
                }
                KeyCode::Left if form.field == 1 => {
                    form.branch_cursor = edit_left(&form.branch_slug, form.branch_cursor);
                }
                KeyCode::Right if form.field == 1 => {
                    form.branch_cursor = edit_right(&form.branch_slug, form.branch_cursor);
                }
                KeyCode::Home if form.field == 1 => {
                    form.branch_cursor = 0;
                }
                KeyCode::End if form.field == 1 => {
                    form.branch_cursor = form.branch_slug.len();
                }
                KeyCode::Delete if form.field == 1 => {
                    let start = form.branch_cursor.min(form.branch_slug.len());
                    let end = edit_right(&form.branch_slug, start);
                    if end > start {
                        form.branch_slug.replace_range(start..end, "");
                    }
                }
                // Plan-mode field — arrows or space toggle plan on/off.
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 4 => {
                    form.plan_mode = !form.plan_mode;
                }
                // Extra-shell field — arrows or space toggle the worktree shell pane.
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 5 => {
                    form.open_shell_pane = !form.open_shell_pane;
                }
                KeyCode::Backspace if matches!(form.field, 1 | 2 | 3) => {
                    if form.field == 1 {
                        let end = form.branch_cursor.min(form.branch_slug.len());
                        let start = edit_left(&form.branch_slug, end);
                        if end > start {
                            form.branch_slug.replace_range(start..end, "");
                            form.branch_cursor = start;
                        }
                        return Ok(());
                    }
                    let target = if form.field == 1 {
                        &mut form.branch_slug
                    } else if form.field == 2 {
                        &mut form.time_estimate
                    } else {
                        &mut form.priority
                    };
                    target.pop();
                }
                KeyCode::Char(c) if matches!(form.field, 1 | 2 | 3) => {
                    if form.field == 1 {
                        let at = form.branch_cursor.min(form.branch_slug.len());
                        form.branch_slug.insert(at, c);
                        form.branch_cursor = edit_right(&form.branch_slug, at);
                        return Ok(());
                    }
                    let target = if form.field == 1 {
                        &mut form.branch_slug
                    } else if form.field == 2 {
                        &mut form.time_estimate
                    } else {
                        &mut form.priority
                    };
                    target.push(c);
                }
                _ => {}
            }
        }
        Mode::TicketOptions(form) => match code {
            KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::Detail,
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.actions.is_empty() {
                    form.selected = (form.selected + 1).min(form.actions.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected = form.selected.saturating_sub(1);
            }
            KeyCode::Enter => app.submit_ticket_option().await?,
            _ => {}
        },
        Mode::DevQaPrompt(form) => match code {
            // Esc cancels the launch; the ticket stays transitioned to Dev QA.
            KeyCode::Esc => app.mode = Mode::Detail,
            // Single toggle field — arrows/space/tab flip worktree ↔ branch-in-repo.
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char(' ')
            | KeyCode::Tab
            | KeyCode::BackTab => {
                form.use_worktree = !form.use_worktree;
            }
            KeyCode::Enter | KeyCode::F(5) => {
                app.submit_devqa_prompt().await?;
            }
            KeyCode::Char(c) if matches!(c, 's' | 'S') && mods.contains(KeyModifiers::CONTROL) => {
                app.submit_devqa_prompt().await?;
            }
            _ => {}
        },
        Mode::DevQaResolveConfirm(_) => match code {
            KeyCode::Esc | KeyCode::Char('n') => app.mode = Mode::Detail,
            KeyCode::Enter | KeyCode::Char('y') => {
                app.submit_devqa_resolve().await?;
            }
            _ => {}
        },
        Mode::DevQaCleanupConfirm(_) => match code {
            // Esc / n keeps the worktree (with its uncommitted changes) in place.
            KeyCode::Esc | KeyCode::Char('n') => {
                app.mode = Mode::Detail;
                app.status = "DevQA worktree kept (uncommitted changes)".into();
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                app.confirm_devqa_cleanup().await?;
            }
            _ => {}
        },
        Mode::EditPriority(form) => match code {
            KeyCode::Esc => app.mode = Mode::Detail,
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.options.is_empty() {
                    form.selected = (form.selected + 1).min(form.options.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected = form.selected.saturating_sub(1);
            }
            KeyCode::Enter => app.submit_priority().await?,
            _ => {}
        },
        Mode::EditTime(form) => match code {
            KeyCode::Esc => app.mode = Mode::Detail,
            KeyCode::Tab => form.field ^= 1,
            KeyCode::Enter => app.submit_edit_time().await?,
            KeyCode::Backspace => {
                let target = if form.field == 0 {
                    &mut form.original_estimate
                } else {
                    &mut form.log_work
                };
                target.pop();
            }
            KeyCode::Char(c) => {
                let target = if form.field == 0 {
                    &mut form.original_estimate
                } else {
                    &mut form.log_work
                };
                target.push(c);
            }
            _ => {}
        },
        Mode::Transition(form) => match code {
            KeyCode::Esc => app.mode = Mode::Detail,
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.options.is_empty() {
                    form.selected = (form.selected + 1).min(form.options.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected = form.selected.saturating_sub(1);
            }
            KeyCode::Enter => app.submit_transition().await?,
            _ => {}
        },
        Mode::ConfluenceSpaces(form) => match code {
            KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::List,
            KeyCode::Char('j') | KeyCode::Down => {
                if !form.spaces.is_empty() {
                    form.selected = (form.selected + 1).min(form.spaces.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                form.selected = form.selected.saturating_sub(1);
            }
            KeyCode::Char('r') => {
                app.open_confluence_spaces().await?;
            }
            KeyCode::Enter => {
                let (key, name) = {
                    let Some(space) = form.spaces.get(form.selected) else {
                        return Ok(());
                    };
                    (space.key.clone(), space.name.clone())
                };
                app.open_confluence_pages(key, name).await?;
            }
            _ => {}
        },
        Mode::ConfluencePages(form) => {
            // Search mode intercepts most keys.
            if form.search_active {
                match code {
                    KeyCode::Esc => {
                        form.search_active = false;
                        form.search_query = String::new();
                        form.search_results = vec![];
                        form.search_error = None;
                        form.search_submitted = false;
                    }
                    KeyCode::Enter if form.search_submitted => {
                        app.open_page_view().await?;
                    }
                    KeyCode::Enter => {
                        app.confluence_search().await?;
                    }
                    KeyCode::Backspace => {
                        form.search_query.pop();
                        form.search_submitted = false;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        let n = form.search_results.len();
                        if n > 0 {
                            form.search_selected = (form.search_selected + 1).min(n - 1);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        form.search_selected = form.search_selected.saturating_sub(1);
                    }
                    KeyCode::Char(c) => {
                        form.search_query.push(c);
                        form.search_submitted = false;
                    }
                    _ => {}
                }
                return Ok(());
            }
            // Normal (non-search) navigation.
            match code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    let has_crumb = !form.breadcrumb.is_empty();
                    if has_crumb {
                        app.confluence_go_back().await?;
                    } else {
                        app.open_confluence_spaces().await?;
                    }
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    let has_crumb = !form.breadcrumb.is_empty();
                    if has_crumb {
                        app.confluence_go_back().await?;
                    } else {
                        app.open_confluence_spaces().await?;
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if !form.pages.is_empty() {
                        form.selected = (form.selected + 1).min(form.pages.len() - 1);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    form.selected = form.selected.saturating_sub(1);
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    app.confluence_drill_down().await?;
                }
                KeyCode::Char('/') => {
                    if let Mode::ConfluencePages(f) = &mut app.mode {
                        f.search_active = true;
                        f.search_query = String::new();
                        f.search_results = vec![];
                        f.search_error = None;
                        f.search_submitted = false;
                    }
                }
                KeyCode::Enter => {
                    app.open_page_view().await?;
                }
                KeyCode::Char('S') => {
                    app.confluence_sync().await?;
                }
                _ => {}
            }
        }

        Mode::PageView(form) => {
            if form.search_active {
                match code {
                    KeyCode::Esc => {
                        let form = match &mut app.mode {
                            Mode::PageView(f) => f,
                            _ => return Ok(()),
                        };
                        form.search_active = false;
                        form.search_query.clear();
                        form.search_matches.clear();
                    }
                    KeyCode::Backspace => {
                        let form = match &mut app.mode {
                            Mode::PageView(f) => f,
                            _ => return Ok(()),
                        };
                        form.search_query.pop();
                        form.search_matches =
                            find_page_search_matches(&form.lines, &form.search_query);
                        form.search_cursor = 0;
                    }
                    KeyCode::Enter | KeyCode::Char('n') => {
                        let form = match &mut app.mode {
                            Mode::PageView(f) => f,
                            _ => return Ok(()),
                        };
                        if !form.search_matches.is_empty() {
                            form.search_cursor =
                                (form.search_cursor + 1) % form.search_matches.len();
                            let target = form.search_matches[form.search_cursor];
                            form.scroll = target.saturating_sub(form.viewport_height / 2);
                        }
                    }
                    KeyCode::Char('N') => {
                        let form = match &mut app.mode {
                            Mode::PageView(f) => f,
                            _ => return Ok(()),
                        };
                        if !form.search_matches.is_empty() {
                            form.search_cursor = form
                                .search_cursor
                                .checked_sub(1)
                                .unwrap_or(form.search_matches.len() - 1);
                            let target = form.search_matches[form.search_cursor];
                            form.scroll = target.saturating_sub(form.viewport_height / 2);
                        }
                    }
                    KeyCode::Char(c) => {
                        let form = match &mut app.mode {
                            Mode::PageView(f) => f,
                            _ => return Ok(()),
                        };
                        form.search_query.push(c);
                        form.search_matches =
                            find_page_search_matches(&form.lines, &form.search_query);
                        form.search_cursor = 0;
                        if let Some(&first) = form.search_matches.first() {
                            form.scroll = first.saturating_sub(form.viewport_height / 2);
                        }
                    }
                    _ => {}
                }
                return Ok(());
            }
            // Normal page-view navigation.
            let max_scroll =
                |form: &PageViewForm| form.lines.len().saturating_sub(form.viewport_height);
            match code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    // Restore previous ConfluencePages mode.
                    let prev = match app.mode {
                        Mode::PageView(ref f) => *f.prev_pages.clone(),
                        _ => return Ok(()),
                    };
                    app.mode = Mode::ConfluencePages(prev);
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Mode::PageView(f) = &mut app.mode {
                        let m = max_scroll(f);
                        f.scroll = (f.scroll + 1).min(m);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Mode::PageView(f) = &mut app.mode {
                        f.scroll = f.scroll.saturating_sub(1);
                    }
                }
                KeyCode::Char('d') | KeyCode::PageDown => {
                    if let Mode::PageView(f) = &mut app.mode {
                        let step = f.viewport_height / 2;
                        let m = max_scroll(f);
                        f.scroll = (f.scroll + step).min(m);
                    }
                }
                KeyCode::Char('u') | KeyCode::PageUp => {
                    if let Mode::PageView(f) = &mut app.mode {
                        let step = f.viewport_height / 2;
                        f.scroll = f.scroll.saturating_sub(step);
                    }
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    if let Mode::PageView(f) = &mut app.mode {
                        f.scroll = 0;
                    }
                }
                KeyCode::Char('G') | KeyCode::End => {
                    if let Mode::PageView(f) = &mut app.mode {
                        let m = max_scroll(f);
                        f.scroll = m;
                    }
                }
                KeyCode::Char('/') => {
                    if let Mode::PageView(f) = &mut app.mode {
                        f.search_active = true;
                        f.search_query.clear();
                        f.search_matches.clear();
                    }
                }
                KeyCode::Char('n') => {
                    if let Mode::PageView(f) = &mut app.mode {
                        if !f.search_matches.is_empty() {
                            f.search_cursor = (f.search_cursor + 1) % f.search_matches.len();
                            let t = f.search_matches[f.search_cursor];
                            let m = max_scroll(f);
                            f.scroll = t.saturating_sub(f.viewport_height / 2).min(m);
                        }
                    }
                }
                KeyCode::Char('N') => {
                    if let Mode::PageView(f) = &mut app.mode {
                        if !f.search_matches.is_empty() {
                            f.search_cursor = f
                                .search_cursor
                                .checked_sub(1)
                                .unwrap_or(f.search_matches.len() - 1);
                            let t = f.search_matches[f.search_cursor];
                            let m = max_scroll(f);
                            f.scroll = t.saturating_sub(f.viewport_height / 2).min(m);
                        }
                    }
                }
                KeyCode::Char('e') => {
                    app.page_view_open_editor().await?;
                }
                KeyCode::Char('S') => {
                    app.page_view_sync().await?;
                }
                _ => {}
            }
        }
        Mode::Tree(_) => match code {
            KeyCode::Char('/') => {
                app.ticket_search_active = true;
                app.ticket_search_query.clear();
                if let Mode::Tree(f) = &mut app.mode {
                    f.selected = 0;
                }
                app.status = "tree search: type key/title, Esc clears".into();
            }
            KeyCode::Esc if app.ticket_search_active || !app.ticket_search_query.is_empty() => {
                app.ticket_search_active = false;
                app.ticket_search_query.clear();
                if let Mode::Tree(f) = &mut app.mode {
                    f.selected = 0;
                }
                app.status = "tree search cleared".into();
            }
            KeyCode::Backspace if app.ticket_search_active => {
                app.ticket_search_query.pop();
                let n = if let Mode::Tree(f) = &app.mode {
                    app.tree_search_visible(f).len()
                } else {
                    0
                };
                if let Mode::Tree(f) = &mut app.mode {
                    f.selected = f.selected.min(n.saturating_sub(1));
                }
            }
            KeyCode::Char(ch)
                if app.ticket_search_active && !mods.contains(KeyModifiers::CONTROL) =>
            {
                app.ticket_search_query.push(ch);
                let n = if let Mode::Tree(f) = &app.mode {
                    app.tree_search_visible(f).len()
                } else {
                    0
                };
                if let Mode::Tree(f) = &mut app.mode {
                    f.selected = f.selected.min(n.saturating_sub(1));
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                app.mode = Mode::List;
            }
            KeyCode::Char('K') => {
                // Same toggle semantics as List view, plus a tree rebuild so
                // the filter+sort actually applies (the K handler at the top
                // of handle_key only flips the flag).
                app.show_completed_prs = !app.show_completed_prs;
                app.status = if app.show_completed_prs {
                    "PRs: showing completed".into()
                } else {
                    "PRs: hiding completed".into()
                };
                app.open_tree().await?;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let n = if let Mode::Tree(f) = &app.mode {
                    app.tree_search_visible(f).len()
                } else {
                    0
                };
                if let Mode::Tree(f) = &mut app.mode {
                    if n > 0 {
                        f.selected = (f.selected + 1).min(n - 1);
                    }
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Mode::Tree(f) = &mut app.mode {
                    f.selected = f.selected.saturating_sub(1);
                }
            }
            KeyCode::Char('g') => {
                if let Mode::Tree(f) = &mut app.mode {
                    f.selected = 0;
                }
            }
            KeyCode::Char('G') => {
                let n = if let Mode::Tree(f) = &app.mode {
                    app.tree_search_visible(f).len()
                } else {
                    0
                };
                if let Mode::Tree(f) = &mut app.mode {
                    if n > 0 {
                        f.selected = n - 1;
                    }
                }
            }
            KeyCode::Char('o') | KeyCode::Tab => {
                let idx = if let Mode::Tree(f) = &app.mode {
                    app.tree_search_visible(f).get(f.selected).copied()
                } else {
                    None
                };
                if let Mode::Tree(f) = &mut app.mode {
                    if let Some(idx) = idx {
                        f.nodes[idx].expanded = !f.nodes[idx].expanded;
                        recompute_tree_visible(f);
                    }
                }
            }
            KeyCode::Char('O') => {
                if let Mode::Tree(f) = &mut app.mode {
                    for n in &mut f.nodes {
                        n.expanded = true;
                    }
                    recompute_tree_visible(f);
                }
            }
            KeyCode::Char('C') => {
                if let Mode::Tree(f) = &mut app.mode {
                    for n in &mut f.nodes {
                        n.expanded = false;
                    }
                    recompute_tree_visible(f);
                }
            }
            KeyCode::Char('v') => {
                if let Mode::Tree(f) = &mut app.mode {
                    f.two_column = !f.two_column;
                }
            }
            KeyCode::Char('c') => {
                let info = if let Mode::Tree(f) = &app.mode {
                    app.tree_search_visible(f)
                        .get(f.selected)
                        .copied()
                        .map(|i| (f.nodes[i].key.clone(), f.nodes[i].issue_type.clone()))
                } else {
                    None
                };
                if let Some((parent_key, parent_type)) = info {
                    let pt = parent_type.as_deref().unwrap_or("");
                    if pt.eq_ignore_ascii_case("sub-task") || pt.eq_ignore_ascii_case("subtask") {
                        app.status = format!("can't add a child under sub-task {}", parent_key);
                        return Ok(());
                    }
                    let project_key = parent_key.split('-').next().unwrap_or("").to_string();
                    let issue_type = if pt.eq_ignore_ascii_case("epic") {
                        "Story"
                    } else {
                        "Sub-task"
                    }
                    .to_string();
                    app.mode = Mode::Create(CreateForm {
                        project: project_key,
                        issue_type,
                        summary: String::new(),
                        description: String::new(),
                        time_estimate: String::new(),
                        priority: String::new(),
                        field: 2, // jump to summary
                        assignee: String::new(),
                        assignee_id: None,
                        assignee_results: vec![],
                        assignee_picker_selected: 0,
                        parent: Some(parent_key),
                        error: None,
                    });
                }
            }
            KeyCode::Enter => {
                let key_opt = if let Mode::Tree(f) = &app.mode {
                    app.tree_search_visible(f)
                        .get(f.selected)
                        .map(|&i| f.nodes[i].key.clone())
                } else {
                    None
                };
                if let Some(key) = key_opt {
                    app.detail_origin = DetailOrigin::List;
                    if let Some(pos) = app
                        .active_idxs
                        .iter()
                        .position(|&i| app.tickets[i].key == key)
                    {
                        app.list_selected = pos;
                    }
                    // Push the TreeForm onto the back-stack so Esc returns here.
                    let prev_mode = std::mem::replace(&mut app.mode, Mode::Detail);
                    if let Mode::Tree(form) = prev_mode {
                        app.nav_stack.push(NavFrame::Tree(Box::new(form)));
                    }
                    let mut stub = Ticket::new_stub();
                    stub.key = key.clone();
                    app.detail = Some(stub);
                    app.status = format!("loading {key}…");
                    app.load_detail().await?;
                }
            }
            _ => {}
        },
        Mode::ArchiveConfirm(_) => match code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                app.mode = Mode::Detail;
            }
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                // If the modal is already showing an error, treat Enter as dismiss.
                let has_error = matches!(&app.mode, Mode::ArchiveConfirm(f) if f.error.is_some());
                if has_error {
                    app.mode = Mode::Detail;
                    return Ok(());
                }
                let key = if let Mode::ArchiveConfirm(f) = &app.mode {
                    f.key.clone()
                } else {
                    return Ok(());
                };
                let mut s = ipc::connect().await?;
                match ipc::send_request(&mut s, &Request::ArchiveTicket { key: key.clone() })
                    .await?
                {
                    Response::Ok => {
                        app.status = format!("archived {key}");
                        app.tickets.retain(|t| t.key != key);
                        app.recompute_indexes();
                        app.detail_focus = DetailFocus::Info;
                        app.subtask_selected = 0;
                        app.detail = None;
                        app.mode = Mode::List;
                        app.refresh().await?;
                    }
                    Response::Err { message } => {
                        if let Mode::ArchiveConfirm(f) = &mut app.mode {
                            f.error = Some(message.clone());
                        }
                        app.status = format!("archive failed: {message}");
                    }
                    _ => {
                        if let Mode::ArchiveConfirm(f) = &mut app.mode {
                            f.error = Some("unexpected daemon response".into());
                        }
                    }
                }
            }
            _ => {}
        },
        Mode::AssignPicker(_) => match code {
            KeyCode::Esc => app.mode = Mode::Detail,
            KeyCode::Up => {
                if let Mode::AssignPicker(f) = &mut app.mode {
                    f.selected = f.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Mode::AssignPicker(f) = &mut app.mode {
                    if !f.results.is_empty() {
                        f.selected = (f.selected + 1).min(f.results.len() - 1);
                    }
                }
            }
            KeyCode::Enter => {
                app.submit_assign_picker().await?;
            }
            KeyCode::Backspace => {
                if let Mode::AssignPicker(f) = &mut app.mode {
                    f.query.pop();
                }
                let q = if let Mode::AssignPicker(f) = &app.mode {
                    f.query.clone()
                } else {
                    String::new()
                };
                if q.len() >= 2 {
                    app.refresh_assign_picker().await?;
                } else if let Mode::AssignPicker(f) = &mut app.mode {
                    f.results.clear();
                    f.selected = 0;
                }
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                if let Mode::AssignPicker(f) = &mut app.mode {
                    f.query.push(c);
                    f.error = None;
                }
                let q = if let Mode::AssignPicker(f) = &app.mode {
                    f.query.clone()
                } else {
                    String::new()
                };
                if q.len() >= 2 {
                    app.refresh_assign_picker().await?;
                }
            }
            _ => {}
        },
        Mode::PrCreate(_) => {
            // Remote-picker sub-modal preempts everything else.
            let in_remote_pick = matches!(&app.mode, Mode::PrCreate(f) if f.remote_pick.is_some());
            if in_remote_pick {
                match code {
                    KeyCode::Esc => {
                        if let Mode::PrCreate(f) = &mut app.mode {
                            f.remote_pick = None;
                        }
                        app.status = "remote pick cancelled".into();
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if let Mode::PrCreate(f) = &mut app.mode {
                            if let Some(p) = &mut f.remote_pick {
                                if !p.items.is_empty() {
                                    p.selected = (p.selected + 1).min(p.items.len() - 1);
                                }
                            }
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if let Mode::PrCreate(f) = &mut app.mode {
                            if let Some(p) = &mut f.remote_pick {
                                p.selected = p.selected.saturating_sub(1);
                            }
                        }
                    }
                    KeyCode::Enter => {
                        app.commit_remote_pick().await?;
                    }
                    _ => {}
                }
                return Ok(());
            }
            // Pending-handle sub-modal: collect a github handle, then continue.
            let in_pending = matches!(&app.mode, Mode::PrCreate(f) if f.pending_handle.is_some());
            if in_pending {
                match code {
                    KeyCode::Esc => app.mode = Mode::Detail,
                    KeyCode::Enter => app.submit_pending_handle().await?,
                    KeyCode::Backspace => {
                        if let Mode::PrCreate(f) = &mut app.mode {
                            if let Some(p) = &mut f.pending_handle {
                                p.handle.pop();
                            }
                        }
                    }
                    KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                        if let Mode::PrCreate(f) = &mut app.mode {
                            if let Some(p) = &mut f.pending_handle {
                                p.handle.push(c);
                            }
                        }
                    }
                    _ => {}
                }
                return Ok(());
            }
            // While the review gate is up, the form is read-only — `y`, `f`,
            // and `Esc` are the only keys that do anything until the user
            // decides to submit, fix, or back out.
            let in_review = matches!(&app.mode, Mode::PrCreate(f) if f.review_state == PrReviewState::Reviewing);
            if in_review {
                // While the /review call is in flight, only Esc has any
                // effect — y/f/R would race the result. Esc clears the
                // pending receiver so the dropped value cancels the task on
                // the daemon side via connection close.
                let pending = app.pending_pr_review.is_some();
                match code {
                    KeyCode::Esc => {
                        if let Mode::PrCreate(f) = &mut app.mode {
                            f.review_state = PrReviewState::Pending;
                            f.review_output = None;
                            f.review_scroll = 0;
                        }
                        app.pending_pr_review = None;
                        let _ = app.save_pr_draft().await;
                        app.status = "review cancelled — keep editing".into();
                    }
                    _ if pending => {
                        // Swallow all other keys until the review returns.
                    }
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        app.submit_pr_create().await?;
                    }
                    KeyCode::Char(c)
                        if matches!(c, 's' | 'S') && mods.contains(KeyModifiers::CONTROL) =>
                    {
                        app.submit_pr_create().await?;
                    }
                    KeyCode::F(5) => app.submit_pr_create().await?,
                    KeyCode::Char('f') | KeyCode::Char('F') => {
                        app.open_pr_fix_session().await?;
                    }
                    KeyCode::Char('R') => {
                        // Re-run /review headlessly; result replaces the
                        // current pane content.
                        app.run_pr_review().await?;
                    }
                    KeyCode::PageDown | KeyCode::Char('J') => {
                        if let Mode::PrCreate(f) = &mut app.mode {
                            f.review_scroll = f.review_scroll.saturating_add(10);
                        }
                    }
                    KeyCode::PageUp | KeyCode::Char('K') => {
                        if let Mode::PrCreate(f) = &mut app.mode {
                            f.review_scroll = f.review_scroll.saturating_sub(10);
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if let Mode::PrCreate(f) = &mut app.mode {
                            f.review_scroll = f.review_scroll.saturating_add(1);
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if let Mode::PrCreate(f) = &mut app.mode {
                            f.review_scroll = f.review_scroll.saturating_sub(1);
                        }
                    }
                    _ => {}
                }
                return Ok(());
            }
            match code {
                KeyCode::Esc => app.mode = Mode::Detail,
                KeyCode::Tab => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        f.field = (f.field + 1) % PrCreateForm::FIELD_COUNT;
                    }
                }
                KeyCode::BackTab => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        f.field = if f.field == 0 {
                            PrCreateForm::FIELD_COUNT - 1
                        } else {
                            f.field - 1
                        };
                    }
                }
                KeyCode::F(5) => app.pr_submit_pressed().await?,
                KeyCode::Char(c)
                    if matches!(c, 's' | 'S') && mods.contains(KeyModifiers::CONTROL) =>
                {
                    app.pr_submit_pressed().await?;
                }
                KeyCode::Enter if mods.contains(KeyModifiers::CONTROL) => {
                    app.pr_submit_pressed().await?;
                }
                // Suggestion accept/reject preempts everything else when a
                // Claude rewrite is on screen.
                KeyCode::Char('y') | KeyCode::Char('Y') if matches!(&app.mode, Mode::PrCreate(f) if f.suggestion.is_some()) =>
                {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        if let Some(s) = f.suggestion.take() {
                            f.body = s;
                            f.body_cursor = f.body.len();
                            f.field = 1;
                            app.status = "body replaced with claude rewrite".into();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Char('n') | KeyCode::Char('N') if matches!(&app.mode, Mode::PrCreate(f) if f.suggestion.is_some()) =>
                {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        f.suggestion = None;
                    }
                    app.status = "rewrite rejected".into();
                    return Ok(());
                }
                // Ctrl-R / F6: ask Claude to tighten the body. Only meaningful
                // on the body field; runs there or surfaces an error.
                KeyCode::Char('r') if mods.contains(KeyModifiers::CONTROL) => {
                    app.improve_pr_body().await?;
                    return Ok(());
                }
                KeyCode::F(6) => {
                    app.improve_pr_body().await?;
                    return Ok(());
                }
                // Up/Down: body field navigates lines; picker fields navigate
                // the dropdown; title is single-line so Up/Down is a no-op.
                KeyCode::Up => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        match f.field {
                            1 => f.body_cursor = edit_up(&f.body, f.body_cursor),
                            2 => {
                                f.reviewer_picker_selected =
                                    f.reviewer_picker_selected.saturating_sub(1)
                            }
                            3 => {
                                f.devqa_picker_selected = f.devqa_picker_selected.saturating_sub(1)
                            }
                            _ => {}
                        }
                    }
                }
                KeyCode::Down => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        match f.field {
                            1 => f.body_cursor = edit_down(&f.body, f.body_cursor),
                            2 => {
                                if !f.reviewer_results.is_empty() {
                                    f.reviewer_picker_selected = (f.reviewer_picker_selected + 1)
                                        .min(f.reviewer_results.len() - 1);
                                }
                            }
                            3 => {
                                if !f.devqa_results.is_empty() {
                                    f.devqa_picker_selected = (f.devqa_picker_selected + 1)
                                        .min(f.devqa_results.len() - 1);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                // Caret moves on title/body. Picker fields keep their text
                // implicitly via the query state — no cursor needed there.
                KeyCode::Left => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        match f.field {
                            0 => f.title_cursor = edit_left(&f.title, f.title_cursor),
                            1 => f.body_cursor = edit_left(&f.body, f.body_cursor),
                            _ => {}
                        }
                    }
                }
                KeyCode::Right => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        match f.field {
                            0 => f.title_cursor = edit_right(&f.title, f.title_cursor),
                            1 => f.body_cursor = edit_right(&f.body, f.body_cursor),
                            _ => {}
                        }
                    }
                }
                KeyCode::Home => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        match f.field {
                            0 => f.title_cursor = 0,
                            1 => f.body_cursor = edit_line_start(&f.body, f.body_cursor),
                            _ => {}
                        }
                    }
                }
                KeyCode::End => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        match f.field {
                            0 => f.title_cursor = f.title.len(),
                            1 => f.body_cursor = edit_line_end(&f.body, f.body_cursor),
                            _ => {}
                        }
                    }
                }
                KeyCode::Enter => {
                    // On reviewer/devqa fields, Enter picks the highlighted user.
                    let mut picked = false;
                    if let Mode::PrCreate(f) = &mut app.mode {
                        match f.field {
                            2 => {
                                if let Some((name, id)) =
                                    f.reviewer_results.get(f.reviewer_picker_selected).cloned()
                                {
                                    f.reviewer = Some((name, id));
                                    f.reviewer_query.clear();
                                    f.reviewer_results.clear();
                                    picked = true;
                                }
                            }
                            3 => {
                                if let Some((name, id)) =
                                    f.devqa_results.get(f.devqa_picker_selected).cloned()
                                {
                                    f.devqa = Some((name, id));
                                    f.devqa_query.clear();
                                    f.devqa_results.clear();
                                    picked = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    if !picked {
                        // Body field inserts newline at caret; other fields advance.
                        if let Mode::PrCreate(f) = &mut app.mode {
                            if f.field == 1 {
                                f.body.insert(f.body_cursor, '\n');
                                f.body_cursor += 1;
                            } else {
                                f.field = (f.field + 1) % PrCreateForm::FIELD_COUNT;
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        match f.field {
                            0 => {
                                if f.title_cursor > 0 {
                                    let prev = edit_left(&f.title, f.title_cursor);
                                    f.title.replace_range(prev..f.title_cursor, "");
                                    f.title_cursor = prev;
                                }
                            }
                            1 => {
                                if f.body_cursor > 0 {
                                    let prev = edit_left(&f.body, f.body_cursor);
                                    f.body.replace_range(prev..f.body_cursor, "");
                                    f.body_cursor = prev;
                                }
                            }
                            2 => {
                                f.reviewer_query.pop();
                            }
                            3 => {
                                f.devqa_query.pop();
                            }
                            _ => {}
                        }
                    }
                    let q = if let Mode::PrCreate(f) = &app.mode {
                        match f.field {
                            2 => Some((true, f.reviewer_query.clone())),
                            3 => Some((false, f.reviewer_query.clone())),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if let Some((rev, query)) = q {
                        if query.len() >= 2 {
                            app.refresh_pr_picker(rev).await?;
                        }
                    }
                }
                KeyCode::Delete => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        match f.field {
                            0 => {
                                if f.title_cursor < f.title.len() {
                                    let nxt = edit_right(&f.title, f.title_cursor);
                                    f.title.replace_range(f.title_cursor..nxt, "");
                                }
                            }
                            1 => {
                                if f.body_cursor < f.body.len() {
                                    let nxt = edit_right(&f.body, f.body_cursor);
                                    f.body.replace_range(f.body_cursor..nxt, "");
                                }
                            }
                            _ => {}
                        }
                    }
                }
                KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                    if let Mode::PrCreate(f) = &mut app.mode {
                        match f.field {
                            0 => {
                                f.title.insert(f.title_cursor, c);
                                f.title_cursor += c.len_utf8();
                            }
                            1 => {
                                f.body.insert(f.body_cursor, c);
                                f.body_cursor += c.len_utf8();
                            }
                            2 => f.reviewer_query.push(c),
                            3 => f.devqa_query.push(c),
                            _ => {}
                        }
                    }
                    let trigger = if let Mode::PrCreate(f) = &app.mode {
                        match f.field {
                            2 if f.reviewer_query.len() >= 2 => Some(true),
                            3 if f.devqa_query.len() >= 2 => Some(false),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if let Some(rev) = trigger {
                        app.refresh_pr_picker(rev).await?;
                    }
                }
                _ => {}
            }
        }
        Mode::ActiveStatusConfig(form) => {
            // Top-level back: Esc/q while NOT in add-mode pops the nav stack
            // (typically back to Settings, which is where the user came from).
            // Handled before the borrow on `form` so the async call doesn't
            // overlap. Add-mode's own Esc cancels the input (handled below).
            if form.adding.is_none() && matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                app.pop_back_or_quit().await?;
                return Ok(());
            }
            // Two-press 'd' guard: any other key clears the pending row.
            if !matches!(code, KeyCode::Char('d')) {
                form.pending_remove = None;
            }
            if let Some(buf) = form.adding.as_mut() {
                // Inline add-input mode.
                match code {
                    KeyCode::Esc => {
                        form.adding = None;
                        app.status = "add cancelled".into();
                    }
                    KeyCode::Enter => {
                        let val = buf.trim().to_string();
                        if val.is_empty() {
                            form.adding = None;
                        } else if form.items.iter().any(|s| s.eq_ignore_ascii_case(&val)) {
                            app.status = format!("\"{val}\" already in list");
                            form.adding = None;
                        } else {
                            form.items.push(val.clone());
                            form.selected = form.items.len() - 1;
                            form.adding = None;
                            if let Err(e) = app.save_active_statuses() {
                                app.status = format!("save err: {e:#}");
                            } else {
                                app.status = format!("added \"{val}\"");
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => buf.push(c),
                    _ => {}
                }
                return Ok(());
            }
            match code {
                KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::List,
                KeyCode::Char('j') | KeyCode::Down => {
                    if !form.items.is_empty() {
                        form.selected = (form.selected + 1).min(form.items.len() - 1);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    form.selected = form.selected.saturating_sub(1);
                }
                KeyCode::Char('i') | KeyCode::Char('a') => {
                    form.adding = Some(String::new());
                    app.status = "type new status · enter: add · esc: cancel".into();
                }
                KeyCode::Char('d') => {
                    if form.items.is_empty() {
                        return Ok(());
                    }
                    let idx = form.selected.min(form.items.len() - 1);
                    if form.pending_remove == Some(idx) {
                        let removed = form.items.remove(idx);
                        form.pending_remove = None;
                        if form.selected >= form.items.len() && !form.items.is_empty() {
                            form.selected = form.items.len() - 1;
                        }
                        if let Err(e) = app.save_active_statuses() {
                            app.status = format!("save err: {e:#}");
                        } else {
                            app.status = format!("removed \"{removed}\"");
                        }
                    } else {
                        form.pending_remove = Some(idx);
                        app.status = "press 'd' again to remove this status".into();
                    }
                }
                _ => {}
            }
        }
        Mode::Settings(_) => {
            // Handled out-of-band so we can drop the outer borrow on `app.mode`
            // before invoking async picker fetches.
        }
        Mode::PrCommentReply(_) => match code {
            KeyCode::Esc => app.mode = Mode::Detail,
            KeyCode::F(5) => app.submit_pr_comment_reply().await?,
            KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => {
                app.submit_pr_comment_reply().await?;
            }
            KeyCode::Enter if mods.contains(KeyModifiers::CONTROL) => {
                app.submit_pr_comment_reply().await?;
            }
            KeyCode::Enter => {
                if let Mode::PrCommentReply(f) = &mut app.mode {
                    f.body.insert(f.body_cursor, '\n');
                    f.body_cursor += 1;
                }
            }
            KeyCode::Backspace => {
                if let Mode::PrCommentReply(f) = &mut app.mode {
                    if f.body_cursor > 0 {
                        let prev = edit_left(&f.body, f.body_cursor);
                        f.body.replace_range(prev..f.body_cursor, "");
                        f.body_cursor = prev;
                    }
                }
            }
            KeyCode::Delete => {
                if let Mode::PrCommentReply(f) = &mut app.mode {
                    if f.body_cursor < f.body.len() {
                        let nxt = edit_right(&f.body, f.body_cursor);
                        f.body.replace_range(f.body_cursor..nxt, "");
                    }
                }
            }
            KeyCode::Left => {
                if let Mode::PrCommentReply(f) = &mut app.mode {
                    f.body_cursor = edit_left(&f.body, f.body_cursor);
                }
            }
            KeyCode::Right => {
                if let Mode::PrCommentReply(f) = &mut app.mode {
                    f.body_cursor = edit_right(&f.body, f.body_cursor);
                }
            }
            KeyCode::Up => {
                if let Mode::PrCommentReply(f) = &mut app.mode {
                    f.body_cursor = edit_up(&f.body, f.body_cursor);
                }
            }
            KeyCode::Down => {
                if let Mode::PrCommentReply(f) = &mut app.mode {
                    f.body_cursor = edit_down(&f.body, f.body_cursor);
                }
            }
            KeyCode::Home => {
                if let Mode::PrCommentReply(f) = &mut app.mode {
                    f.body_cursor = edit_line_start(&f.body, f.body_cursor);
                }
            }
            KeyCode::End => {
                if let Mode::PrCommentReply(f) = &mut app.mode {
                    f.body_cursor = edit_line_end(&f.body, f.body_cursor);
                }
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                if let Mode::PrCommentReply(f) = &mut app.mode {
                    f.body.insert(f.body_cursor, c);
                    f.body_cursor += c.len_utf8();
                }
            }
            _ => {}
        },
        // Rules + RuleEdit + RuleLog + PullRequests + CopilotFixRun + Home are routed via their own dispatch
        // (see handle_key top); these arms only exist for match exhaustiveness.
        Mode::Rules(_)
        | Mode::RuleEdit(_)
        | Mode::RuleLog(_)
        | Mode::PullRequests(_)
        | Mode::CopilotFixRun(_)
        | Mode::Home(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod parse_pr_url_tests {
    use super::parse_pr_url;

    #[test]
    fn standard_url() {
        assert_eq!(
            parse_pr_url("https://github.com/acme/widgets/pull/123"),
            Some(("acme/widgets".to_string(), 123))
        );
    }

    #[test]
    fn trailing_fragment() {
        assert_eq!(
            parse_pr_url("https://github.com/acme/widgets/pull/42#discussion_r1"),
            Some(("acme/widgets".to_string(), 42))
        );
    }

    #[test]
    fn not_a_pr_url() {
        assert_eq!(parse_pr_url("https://github.com/acme/widgets"), None);
        assert_eq!(parse_pr_url("https://example.com/foo/pull/1"), None);
    }
}
