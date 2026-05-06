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
use std::path::PathBuf;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::time::Duration;

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
    if s.is_empty() { None } else { Some(s.to_string()) }
}

fn build_claude_context(t: &Ticket, projects: &[std::path::PathBuf], suggestion: &str) -> String {
    let projects_block = if projects.is_empty() {
        "—".to_string()
    } else {
        projects.iter().map(|p| format!("- {}", p.display())).collect::<Vec<_>>().join("\n")
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
        suggestion = if suggestion.is_empty() { "(none yet)" } else { suggestion },
    )
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
    Implementation(ImplementationForm),
    Projects(ProjectsForm),
    ProjectsAdd(ProjectsAddForm),
    TicketProjects(TicketProjectsForm),
    ConfluenceSpaces(ConfluenceSpacesForm),
    ConfluencePages(ConfluencePagesForm),
}

pub struct ConfluenceSpacesForm {
    pub spaces: Vec<jui_core::confluence_api::ConfluenceSpace>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
}

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
    pub time_estimate: String,
    pub priority: String,
    pub need_time: bool,
    pub need_priority: bool,
    /// 0 → time, 1 → priority. Skipped fields aren't part of cycling.
    pub field: u8,
    pub error: Option<String>,
    /// Valid priorities for this Jira instance (live-fetched). Used both as a hint
    /// and to validate the user's input before sending.
    pub valid_priorities: Vec<String>,
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DetailFocus {
    Info,
    Projects,
    Subtasks,
    Comments,
}

impl DetailFocus {
    pub fn next(self) -> Self {
        match self {
            Self::Info => Self::Projects,
            Self::Projects => Self::Subtasks,
            Self::Subtasks => Self::Comments,
            Self::Comments => Self::Info,
        }
    }
    pub fn prev(self) -> Self {
        match self {
            Self::Info => Self::Comments,
            Self::Projects => Self::Info,
            Self::Subtasks => Self::Projects,
            Self::Comments => Self::Subtasks,
        }
    }
}

/// Two-press deletion target. Either a comment id or a linked project path.
pub enum PendingDelete {
    Comment(String),
    Link(std::path::PathBuf),
}

#[derive(Clone)]
pub struct DetailLinkedProject {
    pub project: ProjectStatus,
    /// "confirmed" | "suggested"
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
    pub field: u8, // 0=project, 1=type, 2=summary, 3=description, 4=estimate, 5=priority
    /// When set, the new issue is created as a child of this parent key (usually a
    /// sub-task of the currently-viewed ticket).
    pub parent: Option<String>,
    /// Last submission error, shown inline so the full message is readable (the
    /// status bar truncates).
    pub error: Option<String>,
}

impl CreateForm {
    pub const FIELD_COUNT: u8 = 6;
    pub fn field_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.project,
            1 => &mut self.issue_type,
            2 => &mut self.summary,
            3 => &mut self.description,
            4 => &mut self.time_estimate,
            _ => &mut self.priority,
        }
    }
}

pub struct EditForm {
    pub key: String,
    pub summary: String,
}

pub struct CommentForm {
    pub key: String,
    pub body: String,
    pub reply_to: Option<ReplyContext>,
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
}

/// Sentinel offset separating `kanban_extra` indices from `tickets` indices in
/// the kanban column vecs. Values >= this come from `App::kanban_extra`.
const KANBAN_EXTRA_OFFSET: usize = 1 << 24;

impl App {
    pub fn new() -> Self {
        Self {
            tickets: vec![],
            active_idxs: vec![],
            inactive_idxs: vec![],
            list_selected: 0,
            archive_selected: 0,
            mode: Mode::List,
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

    pub async fn load_myself(&mut self) -> Result<()> {
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &Request::Myself).await? {
            Response::Myself { info } => self.my_account_id = Some(info.account_id),
            Response::Err { message } => self.status = format!("auth: {message}"),
            _ => {}
        }
        Ok(())
    }

    pub fn comment_is_mine(&self, c: &Comment) -> bool {
        match (&self.my_account_id, &c.account_id) {
            (Some(me), Some(theirs)) => me == theirs,
            _ => false,
        }
    }

    pub async fn delete_selected_comment(&mut self) -> Result<()> {
        let Some(c) = self.comments.get(self.comment_selected) else { return Ok(()) };
        let (Some(id), Some(t)) = (c.id.clone(), self.detail.as_ref().map(|t| t.key.clone()))
            else { return Ok(()) };
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &Request::DeleteComment { key: t, comment_id: id }).await? {
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
                SortMode::Status => ta.status.cmp(&tb.status).then_with(|| tb.updated.cmp(&ta.updated)),
                SortMode::Breadcrumb => {
                    let key_of = |t: &Ticket| -> (String, String, String) {
                        let g = t.grandparent_summary.clone()
                            .or_else(|| t.grandparent_key.clone())
                            .unwrap_or_default();
                        let p = t.parent_summary.clone()
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
            walk(top, 0, &mut rows, &children_of, &self.expanded_parents, &self.tickets);
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
        self.list_selected = self.list_selected.min(self.active_idxs.len().saturating_sub(1));
        self.archive_selected = self.archive_selected.min(self.inactive_idxs.len().saturating_sub(1));
    }

    /// Returns the ticket currently selected in whichever list view is active.
    pub fn current_ticket(&self) -> Option<&Ticket> {
        let (idxs, sel) = match self.mode {
            Mode::List => (&self.active_idxs, self.list_selected),
            Mode::Archive => (&self.inactive_idxs, self.archive_selected),
            Mode::Kanban | Mode::KanbanFilter(_) => {
                let cols = self.kanban_columns();
                let col = cols.get(self.kanban_col)?;
                let card = self.kanban_card_per_col.get(self.kanban_col).copied().unwrap_or(0);
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
            let jql = format!(
                "assignee = \"{}\" AND statusCategory != Done",
                escaped
            );
            let mut s = match ipc::connect().await {
                Ok(s) => s,
                Err(e) => { errors.push(format!("{name}: {e}")); continue; }
            };
            match ipc::send_request(&mut s, &ipc::Request::ListTickets { jql: Some(jql), limit: 100 }).await {
                Ok(ipc::Response::Tickets { items }) => {
                    for t in items {
                        if seen.insert(t.key.clone()) {
                            combined.push(t);
                        }
                    }
                }
                Ok(ipc::Response::Err { message }) => { errors.push(format!("{name}: {message}")); }
                Err(e) => { errors.push(format!("{name}: {e}")); }
                _ => {}
            }
        }
        // Always write results even when some users failed.
        self.kanban_extra = combined;
        if errors.is_empty() {
            self.status = format!("kanban: {} tickets · {} user(s)", self.kanban_extra.len(), names.len());
        } else {
            self.status = format!("kanban: {} tickets · errors: {}", self.kanban_extra.len(), errors.join("; "));
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
            buckets.entry(status).or_default().push(KANBAN_EXTRA_OFFSET + i);
        }
        let mut cols: Vec<(String, Vec<usize>)> = buckets.into_iter().collect();
        cols.sort_by(|a, b| {
            rank(&a.0)
                .cmp(&rank(&b.0))
                .then_with(|| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()))
        });
        cols
    }

    pub async fn refresh(&mut self) -> Result<()> {
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &Request::ListTickets { jql: None, limit: 100 }).await? {
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
                self.comment_selected = self.comment_selected.min(self.comments.len().saturating_sub(1));
            }
            Response::Err { message } => self.status = format!("comments err: {message}"),
            _ => {}
        }
        // Linked projects (local-only relationship from SQLite).
        let mut s = ipc::connect().await?;
        if let Response::TicketProjects { items } = ipc::send_request(
            &mut s,
            &Request::ListTicketProjects { ticket_key: key },
        )
        .await?
        {
            self.detail_linked_projects = items
                .into_iter()
                .filter(|i| i.linked)
                .map(|i| DetailLinkedProject { project: i.project, state: i.state })
                .collect();
            self.linked_project_selected = self
                .linked_project_selected
                .min(self.detail_linked_projects.len().saturating_sub(1));
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

        let need_time = t.original_estimate_seconds.unwrap_or(0) <= 0;
        let need_priority = match t.priority.as_deref() {
            None | Some("") | Some("--") | Some("None") => true,
            _ => false,
        };
        if need_time || need_priority {
            // Fetch valid priorities so the prompt shows correct examples for this
            // instance (some Jiras use Blocker/P1/P2/P3 instead of Highest/High/...).
            let valid_priorities = if need_priority {
                let mut s = ipc::connect().await?;
                match ipc::send_request(&mut s, &Request::ListPriorities).await? {
                    Response::Priorities { items } => items,
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            self.mode = Mode::StartWorkPrompt(StartWorkPromptForm {
                ticket_key: t.key.clone(),
                time_estimate: String::new(),
                priority: String::new(),
                need_time,
                need_priority,
                field: if need_time { 0 } else { 1 },
                error: None,
                valid_priorities,
            });
            return Ok(());
        }
        self.execute_start_work().await
    }

    pub async fn submit_start_work_prompt(&mut self) -> Result<()> {
        let Mode::StartWorkPrompt(form) = &self.mode else { return Ok(()) };
        let key = form.ticket_key.clone();
        let estimate = if form.need_time { trim_to_opt(&form.time_estimate) } else { None };
        let priority = if form.need_priority { trim_to_opt(&form.priority) } else { None };

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
                &Request::EditPriority { key: key.clone(), priority: p.clone() },
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
        self.execute_start_work().await
    }

    async fn execute_start_work(&mut self) -> Result<()> {
        let Some(t) = self.detail.clone().or_else(|| self.current_ticket().cloned()) else {
            return Ok(());
        };
        let key = t.key.clone();
        let mut status_parts: Vec<String> = Vec::new();

        // 1. Transition to "In Dev" (best-effort).
        let mut s = ipc::connect().await?;
        if let Response::Transitions { items } =
            ipc::send_request(&mut s, &Request::ListTransitions { key: key.clone() }).await?
        {
            let target = items
                .iter()
                .find(|t| {
                    t.to_status
                        .as_deref()
                        .map(|s| s.eq_ignore_ascii_case("In Dev"))
                        .unwrap_or(false)
                })
                .or_else(|| {
                    items.iter().find(|t| {
                        let n = t.name.to_ascii_lowercase();
                        n.contains("dev") || n.contains("in progress")
                    })
                });
            if let Some(tr) = target {
                let mut s = ipc::connect().await?;
                let _ = ipc::send_request(
                    &mut s,
                    &Request::Transition { key: key.clone(), to: tr.name.clone() },
                )
                .await;
                status_parts.push(format!("→ {}", tr.to_status.clone().unwrap_or(tr.name.clone())));
            }
        }

        // 2. SCM branch switch.
        let cwd = std::env::current_dir()?;
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::StartWork { key: key.clone(), cwd },
        )
        .await?;
        if let Response::StartWork { reply } = &resp {
            let part = match reply {
                StartWorkReply::GitSwitched { branch, created } => {
                    format!("{} {branch}", if *created { "created" } else { "switched" })
                }
                StartWorkReply::SvnExport { value } => format!("svn export {value}"),
                StartWorkReply::NoScm => "no SCM".into(),
                StartWorkReply::StagedChanges => {
                    self.status = "git has staged changes — commit or stash first".into();
                    return Ok(());
                }
            };
            status_parts.push(part);
        } else if let Response::Err { message } = resp {
            self.status = format!("scm err: {message}");
            return Ok(());
        }

        // 3. Find a linked project to cd into.
        let project_paths: Vec<std::path::PathBuf> = self
            .detail_linked_projects
            .iter()
            .filter(|p| p.project.available)
            .map(|p| p.project.path.clone())
            .collect();
        let Some(top) = project_paths.first().cloned() else {
            self.status = format!(
                "{} · no linked project available — link one (P) and retry",
                status_parts.join(" · ")
            );
            return Ok(());
        };

        // 4. Get or create the Claude session id.
        let mut s = ipc::connect().await?;
        let existing = match ipc::send_request(
            &mut s,
            &Request::GetClaudeSession { ticket_key: key.clone() },
        )
        .await?
        {
            Response::ClaudeSession { session_id } => session_id,
            _ => None,
        };
        let (session_arg, is_resume, session_id) = if let Some(id) = existing {
            (vec!["--resume".into(), id.clone()], true, id)
        } else {
            let new_id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| chrono::Utc::now().timestamp_micros().to_string());
            let mut s = ipc::connect().await?;
            let _ = ipc::send_request(
                &mut s,
                &Request::SaveClaudeSession {
                    ticket_key: key.clone(),
                    session_id: new_id.clone(),
                },
            )
            .await?;
            (vec!["--session-id".into(), new_id.clone()], false, new_id)
        };

        // 5. Pull the cached implementation suggestion (if any) for the context.
        let mut s = ipc::connect().await?;
        let impl_md = match ipc::send_request(
            &mut s,
            &Request::GetImplementation { ticket_key: key.clone() },
        )
        .await?
        {
            Response::Implementation { markdown, .. } => markdown,
            _ => String::new(),
        };

        // 6. Tmux check + width.
        if std::env::var("TMUX").is_err() {
            self.status = format!(
                "{} · not in tmux — run: cd {} && claude {} ({})",
                status_parts.join(" · "),
                top.display(),
                session_arg.join(" "),
                if is_resume { "resume" } else { "new" }
            );
            return Ok(());
        }
        let width = tmux_window_width().unwrap_or(0);

        // 7. Build the launch command. On resume, claude already has prior context;
        // skip piping the file. On new sessions, pipe the context as the first message.
        let session_arg_str = session_arg.join(" ");
        let cmd = if is_resume {
            format!("cd {} && claude {}", shell_escape(&top.display().to_string()), session_arg_str)
        } else {
            let context = build_claude_context(&t, &project_paths, &impl_md);
            let ctx_path = std::env::temp_dir().join(format!("jui-ctx-{}.md", key));
            std::fs::write(&ctx_path, context)?;
            format!(
                "cd {} && cat {} | claude {}",
                shell_escape(&top.display().to_string()),
                shell_escape(&ctx_path.display().to_string()),
                session_arg_str,
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

        // 9. After Claude has settled, send `/remote-control` to enable RC mode.
        // Detached subshell so we don't block the TUI.
        let send_cmd = format!(
            "(sleep 4; tmux send-keys -t {pane} '/remote-control' Enter) >/dev/null 2>&1 &",
            pane = pane_target,
        );
        let _ = std::process::Command::new("sh").arg("-c").arg(&send_cmd).spawn();

        let mode = if is_resume { "resumed" } else { "new" };
        let layout = if width >= 400 { "split" } else { "window" };
        self.status = format!(
            "{} · claude {mode} ({layout}) · session {}",
            status_parts.join(" · "),
            &session_id[..8.min(session_id.len())]
        );
        Ok(())
    }

    pub async fn submit_create(&mut self) -> Result<()> {
        let Mode::Create(form) = &self.mode else { return Ok(()) };
        let description = trim_to_opt(&form.description);
        let priority = trim_to_opt(&form.priority);
        let estimate = trim_to_opt(&form.time_estimate);
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
                &Request::EditPriority { key: new_key.clone(), priority: p.clone() },
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

    pub async fn submit_edit(&mut self) -> Result<()> {
        let Mode::Edit(form) = &self.mode else { return Ok(()) };
        let req = Request::EditSummary { key: form.key.clone(), summary: form.summary.clone() };
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &req).await? {
            Response::Ok => {
                self.status = format!("edited {}", form.key);
                self.mode = Mode::Detail;
                self.load_detail().await?;
            }
            Response::Err { message } => self.status = format!("err: {message}"),
            _ => self.status = "unexpected response".into(),
        }
        Ok(())
    }

    pub async fn submit_comment(&mut self) -> Result<()> {
        let Mode::Comment(form) = &self.mode else { return Ok(()) };
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
        let req = Request::AddComment { key: form.key.clone(), body };
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &req).await? {
            Response::Ok => {
                self.status = format!("commented on {}", form.key);
                self.mode = Mode::Detail;
                self.load_detail().await?;
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
        let resp = ipc::send_request(&mut s, &Request::GetImplementation { ticket_key: key.clone() }).await?;
        let mut form = ImplementationForm {
            key: key.clone(),
            markdown: String::new(),
            project_paths: vec![],
            updated_at: String::new(),
            scroll: 0,
            status_line: String::new(),
        };
        match resp {
            Response::Implementation { markdown, project_paths, updated_at } => {
                form.markdown = markdown;
                form.project_paths = project_paths.into_iter().map(std::path::PathBuf::from).collect();
                form.updated_at = updated_at;
            }
            Response::Err { .. } => {
                // Nothing cached. Trigger generation and show a placeholder.
                let mut s = ipc::connect().await?;
                let _ = ipc::send_request(&mut s, &Request::GenerateImplementation { ticket_key: key }).await?;
                form.status_line = "no cached suggestion — generation queued. press 'r' to reload.".into();
            }
            _ => form.status_line = "unexpected response".into(),
        }
        self.mode = Mode::Implementation(form);
        Ok(())
    }

    pub async fn regenerate_implementation(&mut self) -> Result<()> {
        let Mode::Implementation(form) = &self.mode else { return Ok(()) };
        let key = form.key.clone();
        let mut s = ipc::connect().await?;
        let _ = ipc::send_request(&mut s, &Request::GenerateImplementation { ticket_key: key }).await?;
        if let Mode::Implementation(form) = &mut self.mode {
            form.status_line = "generation queued. press 'r' again later to reload.".into();
        }
        Ok(())
    }

    pub async fn reload_implementation(&mut self) -> Result<()> {
        let Mode::Implementation(form) = &self.mode else { return Ok(()) };
        let key = form.key.clone();
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(&mut s, &Request::GetImplementation { ticket_key: key }).await?;
        if let Mode::Implementation(form) = &mut self.mode {
            match resp {
                Response::Implementation { markdown, project_paths, updated_at } => {
                    form.markdown = markdown;
                    form.project_paths = project_paths.into_iter().map(std::path::PathBuf::from).collect();
                    form.updated_at = updated_at;
                    form.status_line = format!("loaded · updated {updated_at}", updated_at = form.updated_at);
                }
                Response::Err { message } => form.status_line = format!("not yet ready: {message}"),
                _ => form.status_line = "unexpected response".into(),
            }
        }
        Ok(())
    }

    pub async fn save_implementation_to_file(&mut self) -> Result<()> {
        let Mode::Implementation(form) = &self.mode else { return Ok(()) };
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

    pub async fn launch_claude_in_tmux(&mut self) -> Result<()> {
        let Mode::Implementation(form) = &self.mode else { return Ok(()) };
        if std::env::var("TMUX").is_err() {
            self.status = "must be running inside tmux to launch a new window".into();
            return Ok(());
        }
        let Some(top) = form.project_paths.iter().find(|p| p.exists()).cloned() else {
            self.status = "no available project path to cd into".into();
            return Ok(());
        };
        let Some(t) = &self.detail else { return Ok(()) };
        // Write the context to a per-ticket temp file the new claude session reads via cat.
        let tmp_dir = std::env::temp_dir();
        let ctx_path = tmp_dir.join(format!("jui-ctx-{}.md", t.key));
        let context = build_claude_context(t, &form.project_paths, &form.markdown);
        std::fs::write(&ctx_path, context)?;
        // tmux new-window: -c sets the cwd, the shell command pipes the context into
        // claude in interactive mode. Using sh -lc so PATH/aliases resolve normally.
        let cmd = format!(
            "cat {ctx} | claude",
            ctx = shell_escape(&ctx_path.display().to_string())
        );
        let status = std::process::Command::new("tmux")
            .arg("new-window")
            .arg("-c")
            .arg(&top)
            .arg(format!("sh -lc {}", shell_escape(&cmd)))
            .status()?;
        if !status.success() {
            self.status = "tmux new-window failed".into();
        } else {
            self.status = format!("opened claude in new tmux window @ {}", top.display());
            if let Mode::Implementation(form) = &mut self.mode {
                form.status_line = "claude launched in new tmux window".into();
            }
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
        let Mode::EditPriority(form) = &self.mode else { return Ok(()) };
        let Some(name) = form.options.get(form.selected) else { return Ok(()) };
        let key = form.key.clone();
        let priority = name.clone();
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::EditPriority { key: key.clone(), priority: priority.clone() },
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
        let resp = ipc::send_request(&mut s, &Request::ListTicketProjects { ticket_key: key }).await?;
        if let Mode::TicketProjects(form) = &mut self.mode {
            match resp {
                Response::TicketProjects { items } => {
                    form.items = items;
                    if form.items.is_empty() {
                        form.error = Some("no projects configured · open projects (p) and add some first".into());
                    }
                }
                Response::Err { message } => form.error = Some(message),
                _ => form.error = Some("unexpected response".into()),
            }
        }
        Ok(())
    }

    pub async fn toggle_ticket_project(&mut self) -> Result<()> {
        let Mode::TicketProjects(form) = &self.mode else { return Ok(()) };
        let Some(item) = form.items.get(form.selected) else { return Ok(()) };
        let ticket_key = form.ticket_key.clone();
        let path = item.project.path.clone();
        let req = if item.linked {
            Request::UnlinkProject { ticket_key: ticket_key.clone(), project_path: path.clone() }
        } else {
            Request::LinkProject { ticket_key: ticket_key.clone(), project_path: path.clone() }
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
        self.mode = Mode::Projects(ProjectsForm { items, selected: 0, pending_remove: None });
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
            &Request::ScanRepos { root: PathBuf::new(), max_depth: 6 },
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
        let Mode::ProjectsAdd(form) = &self.mode else { return Ok(()) };
        let filtered = form.filtered();
        let Some(idx) = filtered.get(form.selected) else { return Ok(()) };
        let Some(repo) = form.repos.get(*idx) else { return Ok(()) };
        let path = repo.path.clone();
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(
            &mut s,
            &Request::AddProject { path: path.clone(), nickname: None },
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
        let Mode::Projects(form) = &self.mode else { return Ok(()) };
        let Some(p) = form.items.get(form.selected) else { return Ok(()) };
        let path = p.path.clone();
        let mut s = ipc::connect().await?;
        let resp = ipc::send_request(&mut s, &Request::RemoveProject { path: path.clone() }).await?;
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
        let Mode::EditTime(form) = &self.mode else { return Ok(()) };
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
        let Mode::Transition(form) = &self.mode else { return Ok(()) };
        let Some(opt) = form.options.get(form.selected) else { return Ok(()) };
        let key = form.key.clone();
        // jira-cli's `issue move` matches against the *transition name* (e.g. "Start Code
        // Review"), not the destination status (e.g. "Code Rvw"). They're often the same
        // string, but not always.
        let target = opt.name.clone();
        let req = Request::Transition { key: key.clone(), to: target.clone() };
        let mut s = ipc::connect().await?;
        match ipc::send_request(&mut s, &req).await? {
            Response::Ok => {
                self.status = format!("{} → {}", key, target);
                self.mode = Mode::Detail;
                self.load_detail().await?;
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
        let api = match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
            Ok(a) => a,
            Err(e) => {
                if let Mode::ConfluenceSpaces(form) = &mut self.mode {
                    form.loading = false;
                    form.error = Some(format!("{e:#}"));
                }
                return Ok(());
            }
        };
        let result = api.list_spaces().await;
        if let Mode::ConfluenceSpaces(form) = &mut self.mode {
            form.loading = false;
            match result {
                Ok(spaces) => form.spaces = spaces,
                Err(e) => form.error = Some(format!("{e:#}")),
            }
        }
        Ok(())
    }

    pub async fn open_confluence_pages(&mut self, space_key: String, space_name: String) -> Result<()> {
        self.mode = Mode::ConfluencePages(ConfluencePagesForm {
            space_key: space_key.clone(),
            space_name,
            breadcrumb: vec![],
            pages: vec![],
            selected: 0,
            loading: true,
            error: None,
        });
        let api = match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
            Ok(a) => a,
            Err(e) => {
                if let Mode::ConfluencePages(form) = &mut self.mode {
                    form.loading = false;
                    form.error = Some(format!("{e:#}"));
                }
                return Ok(());
            }
        };
        let result = api.list_pages(&space_key).await;
        if let Mode::ConfluencePages(form) = &mut self.mode {
            form.loading = false;
            match result {
                Ok(pages) => form.pages = pages,
                Err(e) => form.error = Some(format!("{e:#}")),
            }
        }
        Ok(())
    }

    pub async fn confluence_drill_down(&mut self) -> Result<()> {
        let page_id = {
            let Mode::ConfluencePages(form) = &mut self.mode else { return Ok(()) };
            let Some(page) = form.pages.get(form.selected) else { return Ok(()) };
            let id = page.id.clone();
            let title = page.title.clone();
            form.breadcrumb.push((id.clone(), title));
            form.loading = true;
            form.error = None;
            id
        };
        let api = match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
            Ok(a) => a,
            Err(e) => {
                if let Mode::ConfluencePages(form) = &mut self.mode {
                    form.loading = false;
                    form.error = Some(format!("{e:#}"));
                    form.breadcrumb.pop();
                }
                return Ok(());
            }
        };
        let result = api.get_children(&page_id).await;
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
            let Mode::ConfluencePages(form) = &mut self.mode else { return Ok(()) };
            form.breadcrumb.pop();
            let parent_id = form.breadcrumb.last().map(|(id, _)| id.clone());
            form.loading = true;
            form.error = None;
            (form.space_key.clone(), parent_id)
        };
        let api = match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
            Ok(a) => a,
            Err(e) => {
                if let Mode::ConfluencePages(form) = &mut self.mode {
                    form.loading = false;
                    form.error = Some(format!("{e:#}"));
                }
                return Ok(());
            }
        };
        let result = match parent_id {
            Some(ref id) => api.get_children(id).await,
            None => api.list_pages(&space_key).await,
        };
        if let Mode::ConfluencePages(form) = &mut self.mode {
            form.loading = false;
            form.selected = 0;
            match result {
                Ok(pages) => form.pages = pages,
                Err(e) => form.error = Some(format!("{e:#}")),
            }
        }
        Ok(())
    }

    pub async fn confluence_open_in_editor(&mut self) -> Result<()> {
        let page_id = {
            let Mode::ConfluencePages(form) = &self.mode else { return Ok(()) };
            let Some(page) = form.pages.get(form.selected) else { return Ok(()) };
            page.id.clone()
        };
        self.status = "fetching page…".into();
        let api = match jui_core::confluence_api::ConfluenceApi::from_jira_config() {
            Ok(a) => a,
            Err(e) => {
                self.status = format!("config error: {e:#}");
                return Ok(());
            }
        };
        match api.get_page_html(&page_id).await {
            Err(e) => {
                self.status = format!("fetch error: {e:#}");
            }
            Ok((title, html)) => {
                let markdown = html_to_markdown(&html);
                let path = format!("/tmp/confluence-{}.md", page_id);
                let content = format!("# {}\n\n{}", title, markdown);
                std::fs::write(&path, &content)?;
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
                crossterm::terminal::disable_raw_mode()?;
                crossterm::execute!(
                    std::io::stdout(),
                    crossterm::terminal::LeaveAlternateScreen
                )?;
                let _ = std::process::Command::new(&editor).arg(&path).status();
                crossterm::terminal::enable_raw_mode()?;
                crossterm::execute!(
                    std::io::stdout(),
                    crossterm::terminal::EnterAlternateScreen
                )?;
                self.needs_clear = true;
                self.status = format!("opened '{}' in {}", title, editor);
            }
        }
        Ok(())
    }
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
    // Try python3 html2text.
    if let Ok(mut child) = std::process::Command::new("python3")
        .args(["-c", "import sys,html2text; print(html2text.html2text(sys.stdin.read()))"])
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
    // Fallback: strip tags.
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
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
    if let Err(e) = app.refresh().await {
        app.status = format!("refresh failed: {e:#}");
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
        term.draw(|f| ui::draw(f, app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                let was_list = matches!(app.mode, Mode::List | Mode::Archive);
                handle_key(app, k.code, k.modifiers).await?;
                let now_list = matches!(app.mode, Mode::List | Mode::Archive);
                if now_list && !was_list {
                    if let Err(e) = app.refresh().await {
                        app.status = format!("refresh failed: {e:#}");
                    }
                }
            }
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
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
                });
            }
        }
        KeyCode::Char('d') => {
            let Some(c) = app.comments.get(app.comment_selected) else { return Ok(()) };
            if !app.comment_is_mine(c) {
                app.status = "can't delete: not your comment".into();
                app.pending_delete = None;
            } else if let Some(id) = c.id.clone() {
                let confirm = matches!(&app.pending_delete, Some(PendingDelete::Comment(p)) if *p == id);
                if confirm {
                    app.pending_delete = None;
                    app.delete_selected_comment().await?;
                } else {
                    app.pending_delete = Some(PendingDelete::Comment(id));
                    app.status = "press 'd' again to delete this comment, or any other key to cancel".into();
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn detail_subtasks_keys(app: &mut App, code: KeyCode) -> Result<()> {
    let n = app.detail.as_ref().map(|t| t.subtasks.len()).unwrap_or(0);
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if n > 0 {
                app.subtask_selected = (app.subtask_selected + 1).min(n - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.subtask_selected = app.subtask_selected.saturating_sub(1);
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
                    parent: Some(t.key.clone()),
                    error: None,
                });
            }
        }
        KeyCode::Enter => {
            // Drill into the selected subtask (replaces self.detail with that ticket).
            let key = app
                .detail
                .as_ref()
                .and_then(|t| t.subtasks.get(app.subtask_selected))
                .map(|s| s.key.clone());
            if let Some(k) = key {
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
        KeyCode::Char('a') | KeyCode::Char('+') => { app.open_ticket_projects().await?; }
        KeyCode::Char('y') => {
            // Approve a suggested project.
            let Some(p) = app.detail_linked_projects.get(app.linked_project_selected) else { return Ok(()) };
            if p.state == "no_match" {
                app.status = "nothing to approve — claude couldn't find a match".into();
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
            let Some(p) = app.detail_linked_projects.get(app.linked_project_selected) else { return Ok(()) };
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
            let confirm = matches!(&app.pending_delete, Some(PendingDelete::Link(pp)) if *pp == path);
            if confirm {
                app.pending_delete = None;
                if let Some(t) = &app.detail {
                    let key = t.key.clone();
                    let mut s = ipc::connect().await?;
                    let resp = ipc::send_request(
                        &mut s,
                        &Request::UnlinkProject { ticket_key: key, project_path: path.clone() },
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
        app.should_quit = true;
        return Ok(());
    }
    match &mut app.mode {
        Mode::List => match code {
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => {
                if !app.active_idxs.is_empty() {
                    app.list_selected = (app.list_selected + 1).min(app.active_idxs.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.list_selected = app.list_selected.saturating_sub(1);
            }
            KeyCode::Char('r') => { app.refresh().await?; }
            KeyCode::Char('o') => {
                app.sort_mode = app.sort_mode.next();
                app.recompute_indexes();
                app.status = format!("sort: {}", app.sort_mode.label());
            }
            KeyCode::Char('s') => { app.start_work().await?; }
            KeyCode::Char('a') => { app.mode = Mode::Archive; }
            KeyCode::Char('b') => {
                let cols = app.kanban_columns();
                app.kanban_card_per_col = vec![0; cols.len()];
                app.kanban_col = app.kanban_col.min(cols.len().saturating_sub(1));
                app.mode = Mode::Kanban;
            }
            KeyCode::Char('p') => { app.open_projects().await?; }
            KeyCode::Char('f') => { app.open_confluence_spaces().await?; }
            KeyCode::Tab => {
                if let Some(&t_idx) = app.active_idxs.get(app.list_selected) {
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
                    parent: None,
                    error: None,
                });
            }
            KeyCode::Enter => {
                if app.current_ticket().is_some() {
                    app.detail_origin = DetailOrigin::List;
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
            KeyCode::Char('r') => { app.refresh().await?; }
            KeyCode::Char('o') => {
                app.sort_mode = app.sort_mode.next();
                app.recompute_indexes();
                app.status = format!("sort: {}", app.sort_mode.label());
            }
            KeyCode::Enter => {
                if app.current_ticket().is_some() {
                    app.detail_origin = DetailOrigin::Archive;
                    app.load_detail().await?;
                    app.mode = Mode::Detail;
                }
            }
            _ => {}
        },
        Mode::Kanban => {
            let cols = app.kanban_columns();
            if app.kanban_card_per_col.len() != cols.len() {
                app.kanban_card_per_col.resize(cols.len(), 0);
            }
            match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('b') => {
                    app.kanban_expanded_col = None;
                    app.mode = Mode::List;
                }
                KeyCode::Char('r') => { app.refresh().await?; }
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
                    if let Ok(ipc::Response::Users { items, from_cache }) =
                        ipc::send_request(&mut s, &ipc::Request::SearchUsers { query: String::new() }).await
                    {
                        form.results = items.into_iter().map(|u| (u.display_name, u.account_id)).collect();
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
                                app.kanban_assignee_filter.iter().cloned().collect::<Vec<_>>(),
                            )
                        } else { (String::new(), vec![]) };
                        if !save_name.trim().is_empty() && !members.is_empty() {
                            let mut s = ipc::connect().await?;
                            let _ = ipc::send_request(&mut s, &ipc::Request::SaveTeam {
                                name: save_name.trim().to_string(),
                                members,
                            }).await;
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
                KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) && !app.kanban_assignee_filter.is_empty() => {
                    if let Mode::KanbanFilter(ref mut form) = app.mode {
                        form.save_name = Some(String::new());
                    }
                }
                KeyCode::Char('d') if mods.contains(KeyModifiers::CONTROL) => {
                    // Delete team if cursor on a team row.
                    if selected < n_teams {
                        let team_name = if let Mode::KanbanFilter(ref form) = app.mode {
                            form.teams.get(form.selected).map(|t| t.name.clone())
                        } else { None };
                        if let Some(name) = team_name {
                            let mut s = ipc::connect().await?;
                            let _ = ipc::send_request(&mut s, &ipc::Request::DeleteTeam { name }).await;
                            let mut s2 = ipc::connect().await?;
                            if let Ok(ipc::Response::Teams { items }) =
                                ipc::send_request(&mut s2, &ipc::Request::ListTeams).await
                            {
                                if let Mode::KanbanFilter(ref mut form) = app.mode {
                                    form.teams = items;
                                    form.selected = form.selected.min(form.total_rows().saturating_sub(1));
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
                        } else { None };
                        if let Some(members) = members {
                            app.kanban_assignee_filter = members.into_iter().collect();
                        }
                    } else {
                        // Toggle individual user.
                        let user_idx = selected - n_teams;
                        let name = if let Mode::KanbanFilter(ref form) = app.mode {
                            form.results.get(user_idx).map(|(n, _)| n.clone())
                        } else { None };
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
                            form.results = items.into_iter().map(|u| (u.display_name, u.account_id)).collect();
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
                            form.results = items.into_iter().map(|u| (u.display_name, u.account_id)).collect();
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
            // Tab cycles focus regardless of which pane is active.
            if matches!(code, KeyCode::Tab) {
                app.detail_focus = app.detail_focus.next();
                return Ok(());
            }
            if matches!(code, KeyCode::BackTab) {
                app.detail_focus = app.detail_focus.prev();
                return Ok(());
            }
            // Esc / q always exit to the list, regardless of focus.
            if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                app.mode = match app.detail_origin {
                    DetailOrigin::List => Mode::List,
                    DetailOrigin::Archive => Mode::Archive,
                    DetailOrigin::Kanban => Mode::Kanban,
                };
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
                        app.mode = Mode::Edit(EditForm {
                            key: t.key.clone(),
                            summary: t.summary.clone(),
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
                KeyCode::Char('P') => {
                    app.open_ticket_projects().await?;
                    return Ok(());
                }
                KeyCode::Char('s') => {
                    app.start_work().await?;
                    return Ok(());
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
                            field: 2,
                            parent: Some(t.key.clone()),
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
                DetailFocus::Info => {}
            }
            // Info-pane / global keys (the actions on the ticket itself).
            match code {
            KeyCode::Char('e') => {
                if let Some(t) = &app.detail {
                    app.mode = Mode::Edit(EditForm {
                        key: t.key.clone(),
                        summary: t.summary.clone(),
                    });
                }
            }
            KeyCode::Char('t') => {
                if let Some(t) = &app.detail {
                    let key = t.key.clone();
                    app.open_transition(key).await?;
                }
            }
            KeyCode::Char('s') => { app.start_work().await?; }
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
            KeyCode::Char('P') => { app.open_ticket_projects().await?; }
            KeyCode::Char('i') => { app.open_priority_picker().await?; }
            KeyCode::Char('C') => { app.open_implementation().await?; }
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
                    });
                }
            }
            _ => {}
            }
        },
        Mode::Create(form) => match code {
            KeyCode::Esc => app.mode = Mode::List,
            KeyCode::Tab => form.field = (form.field + 1) % CreateForm::FIELD_COUNT,
            KeyCode::BackTab => {
                form.field = if form.field == 0 {
                    CreateForm::FIELD_COUNT - 1
                } else {
                    form.field - 1
                };
            }
            KeyCode::F(5) => { app.submit_create().await?; }
            KeyCode::Char(c) if matches!(c, 's' | 'S') && mods.contains(KeyModifiers::CONTROL) => {
                app.submit_create().await?;
            }
            KeyCode::Enter if mods.contains(KeyModifiers::CONTROL) => {
                app.submit_create().await?;
            }
            KeyCode::Enter => {
                if form.field == CreateForm::FIELD_COUNT - 1 {
                    app.submit_create().await?;
                } else {
                    form.field += 1;
                }
            }
            KeyCode::Backspace => {
                form.field_mut().pop();
            }
            KeyCode::Char(c) => {
                form.field_mut().push(c);
            }
            _ => {}
        },
        Mode::Edit(form) => match code {
            KeyCode::Esc => app.mode = Mode::Detail,
            KeyCode::Enter => app.submit_edit().await?,
            KeyCode::Backspace => { form.summary.pop(); }
            KeyCode::Char(c) => form.summary.push(c),
            _ => {}
        },
        Mode::Comment(form) => match code {
            KeyCode::Esc => app.mode = Mode::Detail,
            KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => app.submit_comment().await?,
            KeyCode::Enter => form.body.push('\n'),
            KeyCode::Backspace => { form.body.pop(); }
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
                KeyCode::Char('a') => { app.open_projects_add().await?; }
                KeyCode::Char('r') => { app.open_projects().await?; }
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
            KeyCode::Esc => { app.open_projects().await?; }
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
            KeyCode::Char('j') | KeyCode::Down => { form.scroll = form.scroll.saturating_add(1); }
            KeyCode::Char('k') | KeyCode::Up => { form.scroll = form.scroll.saturating_sub(1); }
            KeyCode::PageDown => { form.scroll = form.scroll.saturating_add(10); }
            KeyCode::PageUp => { form.scroll = form.scroll.saturating_sub(10); }
            KeyCode::Char('g') => { form.scroll = 0; }
            KeyCode::Char('s') => { app.save_implementation_to_file().await?; }
            KeyCode::Char('o') => { app.launch_claude_in_tmux().await?; }
            KeyCode::Char('r') => { app.reload_implementation().await?; }
            KeyCode::Char('R') => { app.regenerate_implementation().await?; }
            _ => {}
        },
        Mode::StartWorkPrompt(form) => match code {
            KeyCode::Esc => app.mode = Mode::Detail,
            KeyCode::Tab | KeyCode::BackTab => {
                // Toggle between time and priority fields if both are needed.
                if form.need_time && form.need_priority {
                    form.field = if form.field == 0 { 1 } else { 0 };
                }
            }
            KeyCode::F(5) => { app.submit_start_work_prompt().await?; }
            KeyCode::Char(c) if matches!(c, 's' | 'S') && mods.contains(KeyModifiers::CONTROL) => {
                app.submit_start_work_prompt().await?;
            }
            KeyCode::Enter => { app.submit_start_work_prompt().await?; }
            KeyCode::Backspace => {
                let target = if form.field == 0 { &mut form.time_estimate } else { &mut form.priority };
                target.pop();
            }
            KeyCode::Char(c) => {
                let target = if form.field == 0 { &mut form.time_estimate } else { &mut form.priority };
                target.push(c);
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
                let target = if form.field == 0 { &mut form.original_estimate } else { &mut form.log_work };
                target.pop();
            }
            KeyCode::Char(c) => {
                let target = if form.field == 0 { &mut form.original_estimate } else { &mut form.log_work };
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
            KeyCode::Char('r') => { app.open_confluence_spaces().await?; }
            KeyCode::Enter => {
                let (key, name) = {
                    let Some(space) = form.spaces.get(form.selected) else { return Ok(()) };
                    (space.key.clone(), space.name.clone())
                };
                app.open_confluence_pages(key, name).await?;
            }
            _ => {}
        },
        Mode::ConfluencePages(form) => match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                let has_crumb = !form.breadcrumb.is_empty();
                if has_crumb {
                    app.confluence_go_back().await?;
                } else {
                    app.open_confluence_spaces().await?;
                }
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
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
            KeyCode::Enter => {
                app.confluence_open_in_editor().await?;
            }
            _ => {}
        },
    }
    Ok(())
}
