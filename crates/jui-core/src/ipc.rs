use crate::jira_api::{MyselfInfo, TransitionOption, UserInfo};
use crate::scm::RepoEntry;
use crate::ticket::{Comment, Ticket};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Length-prefixed JSON: 4-byte big-endian length, then JSON body.
const MAX_FRAME: u32 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Ping,
    ListTickets { jql: Option<String>, limit: u32 },
    GetTicket { key: String },
    /// Cache-only batch fetch + ancestor walk. Used by Tree mode to avoid N
    /// individual round-trips. Daemon's hourly warmup task keeps the cache
    /// populated with parents.
    GetTicketsWithAncestors { keys: Vec<String> },
    /// Open tickets where the user is the **reviewer** (per the configured
    /// reviewer custom field) or has been **@-mentioned** in the description /
    /// comments — and is *not* the assignee. Daemon issues both queries and
    /// returns them tagged so the TUI can render `[R]` vs `[@]` badges.
    ListMyMentions,
    /// Open a GitHub PR for the worktree associated with `ticket_key`.
    /// Reviewer + DevQA are Jira account ids; daemon resolves them to GitHub
    /// handles via the persistent users map. Submits the PR, requests the
    /// reviewer on GitHub, posts a Jira comment, and transitions the ticket
    /// to "Code Review".
    CreatePullRequest {
        ticket_key: String,
        title: String,
        body: String,
        reviewer_account_id: Option<String>,
        devqa_account_id: Option<String>,
        /// Git remote to push to. `None` means "use the value cached for this
        /// project (or die loudly so the TUI runs its picker)" — daemon
        /// resolves it. Set when the TUI already ran the picker for this PR.
        #[serde(default)]
        push_remote: Option<String>,
    },
    /// Persist a Jira `account_id` → GitHub handle mapping. Used when the user
    /// picks a teammate in the PR-create modal who isn't yet in the map.
    SetGithubHandle { account_id: String, handle: String },
    /// Lookup a single mapping. Returns `Response::GithubHandle { handle }` —
    /// `handle` empty when not in the map.
    GetGithubHandle { account_id: String },
    /// Cached PR comments for the given Jira ticket (only present when the
    /// ticket is associated with an open PR — daemon populates during the
    /// github-mentions refresh).
    ListPrComments { ticket_key: String },
    /// Set up a DevQA worktree for the given PR: locate the user's local clone
    /// of `repo`, fetch `pull/<pr_number>/head` into a local branch, and
    /// `git worktree add` it at `<repo>/../<repo>-worktrees/<ticket_key>-devqa`.
    /// Returns the path and the local branch name so the TUI can open a tmux
    /// pane + Claude with PR context.
    SetupDevQaWorktree {
        ticket_key: String,
        repo: String,        // "<owner>/<name>"
        pr_number: u64,
    },
    /// Set the user-managed PR review state for the given ticket. Valid values:
    /// "awaiting", "reviewing", "completed". Persisted via SQLite so the state
    /// survives daemon restarts.
    SetPrUserState { ticket_key: String, state: String },
    /// Map of ticket_key → state for every PR the user has touched. Tickets
    /// that have never been touched return `Awaiting` by convention (TUI default).
    GetPrUserStates,
    DeleteTicket { key: String },
    /// Transition the ticket to a "closed" state. Daemon picks the first available
    /// transition matching (case-insensitive): Won't Do, Cancelled, Closed, Done.
    /// Used by the TUI's archive flow when the user lacks delete permission on Jira.
    ArchiveTicket { key: String },
    AssignTicket { key: String, assignee: String },
    SetReviewer { key: String, assignee_id: String },
    Refresh { jql: Option<String> },
    StartWork { key: String, cwd: PathBuf },
    AddComment { key: String, body: String },
    ListComments { key: String },
    DeleteComment { key: String, comment_id: String },
    Myself,
    ListProjects,
    AddProject { path: PathBuf, nickname: Option<String> },
    RemoveProject { path: PathBuf },
    /// Recursively scan `root` for git/svn repos. Limited depth.
    ScanRepos { root: PathBuf, max_depth: u32 },
    /// Local-only ticket↔project link. Stored in SQLite, never sent to Jira.
    LinkProject { ticket_key: String, project_path: PathBuf },
    UnlinkProject { ticket_key: String, project_path: PathBuf },
    /// Returns every configured project plus a `linked` flag for this ticket.
    ListTicketProjects { ticket_key: String },
    /// Promote a 'suggested' link to 'confirmed'.
    ConfirmSuggestion { ticket_key: String, project_path: PathBuf },
    /// Mark as 'rejected' so we never re-suggest it.
    RejectSuggestion { ticket_key: String, project_path: PathBuf },
    /// Manually trigger the suggestion worker for a ticket (mostly for testing).
    SuggestProject { ticket_key: String },
    /// Returns the cached implementation suggestion for a ticket, if any.
    GetImplementation { ticket_key: String },
    /// Force regeneration of the implementation suggestion.
    GenerateImplementation { ticket_key: String },
    GetClaudeSession { ticket_key: String },
    SaveClaudeSession { ticket_key: String, session_id: String },
    Transition { key: String, to: String },
    ListTransitions { key: String },
    /// Fetch the instance's full set of status names (deduped, alpha-sorted).
    /// Used by the Settings status-pickers.
    ListStatuses,
    /// Resolve the on-disk worktree path for a ticket's first linked project.
    /// Used by flows (PR review gate, comment chat) that need to spawn Claude
    /// inside the worktree without going through the full StartWork dance.
    GetTicketWorktree { ticket_key: String },
    /// Persisted PR-draft handling. Lets the user resume a review-gated PR
    /// across restarts so an aborted `/review` doesn't lose the form state.
    GetPrDraft { ticket_key: String },
    SavePrDraft { ticket_key: String, draft: crate::cache::PrDraft },
    DeletePrDraft { ticket_key: String },
    /// Run `/review` headlessly against the ticket's worktree, attached to the
    /// ticket's stored Claude session (created if missing). Returns the raw
    /// markdown Claude printed so the TUI can render it inline.
    CodeReview { ticket_key: String },
    /// `git remote -v` parsed against the ticket's worktree (or linked project
    /// root if the worktree dir is missing). Returns (name, fetch_url) pairs.
    ListWorktreeRemotes { ticket_key: String },
    /// Per-project preferred push remote — saved once during the PR-create
    /// picker; reused on subsequent PRs for the same linked project.
    GetPushRemote { ticket_key: String },
    SetPushRemote { ticket_key: String, remote_name: String },
    CreateTicket {
        project: String,
        issue_type: String,
        summary: String,
        body: Option<String>,
        /// When Some, jira-cli is invoked with `-P <parent>` so the new issue is created
        /// as a child (typically a sub-task or a story under an epic).
        #[serde(default)]
        parent: Option<String>,
    },
    EditSummary { key: String, summary: String },
    EditDescription { key: String, body: String },
    /// Ask Claude to rewrite a description more tightly. Pure transform — no Jira write.
    ImproveDescription { summary: String, body: String },
    EditPriority { key: String, priority: String },
    ListPriorities,
    SetEstimate { key: String, original: Option<String>, remaining: Option<String> },
    LogWork { key: String, time_spent: String, comment: Option<String>, new_estimate: Option<String> },
    PendingNotifications,
    Status,
    Shutdown,
    SearchUsers { query: String },
    SaveTeam { name: String, members: Vec<String> },
    ListTeams,
    DeleteTeam { name: String },
    ConfluenceListSpaces,
    /// `parent_id = None` → root pages of space; `Some(id)` → children of that page.
    ConfluenceListPages { space_key: String, parent_id: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Tickets { items: Vec<Ticket> },
    /// Result lists for `ListMyMentions`. Reviewer wins over GitHub wins over
    /// Mentioned on overlap; daemon dedupes by key before returning.
    /// `authored` is just ticket keys of open PRs the user authored — used to
    /// flag the corresponding rows in tree/list views (no ticket payload since
    /// the user's own tickets are already loaded).
    MyMentions {
        reviewing: Vec<Ticket>,
        mentioned: Vec<Ticket>,
        github: Vec<Ticket>,
        #[serde(default)]
        authored: Vec<String>,
    },
    /// Reply for `CreatePullRequest`.
    PullRequestCreated { url: String, number: u64 },
    /// Reply for `GetGithubHandle`. Empty string == no mapping.
    GithubHandle { handle: String },
    /// Reply for `ListPrComments`. `pr_link` is the canonical PR URL tied to
    /// the ticket (populated even when there are zero comments); `None` when
    /// no PR is associated.
    PrComments {
        items: Vec<crate::github::PrComment>,
        #[serde(default)]
        pr_link: Option<String>,
    },
    /// Reply for `SetupDevQaWorktree`.
    DevQaWorktree { path: std::path::PathBuf, branch: String },
    /// Reply for `GetPrUserStates`.
    PrUserStates { items: std::collections::HashMap<String, String> },
    Ticket { ticket: Ticket },
    StartWork { reply: StartWorkReply },
    Created { key: String },
    Notifications { items: Vec<NotificationItem> },
    Status { status: DaemonStatus },
    Transitions { items: Vec<TransitionOption> },
    Statuses { items: Vec<String> },
    /// `path = None` when no linked project or the slug's worktree dir is
    /// missing; caller surfaces the appropriate hint to the user.
    TicketWorktree { path: Option<PathBuf> },
    /// `draft = None` when no in-flight PR draft is on file for this ticket.
    PrDraft { draft: Option<crate::cache::PrDraft> },
    /// Reply for `CodeReview`. Markdown body from `claude -p /review`.
    ReviewOutput { markdown: String },
    /// Reply for `ListWorktreeRemotes`.
    Remotes { items: Vec<(String, String)> },
    /// Reply for `GetPushRemote`. `name = None` means no override on file
    /// (caller falls back to the picker flow).
    PushRemote { name: Option<String> },
    Comments { items: Vec<Comment> },
    Priorities { items: Vec<String> },
    Implementation { markdown: String, project_paths: Vec<String>, updated_at: String },
    /// Reply for `ImproveDescription`.
    Improved { body: String },
    /// Sent immediately when a generation request was queued; the markdown shows up on
    /// a subsequent GetImplementation call.
    Queued,
    ClaudeSession { session_id: Option<String> },
    Myself { info: MyselfInfo },
    Projects { items: Vec<ProjectStatus> },
    Repos { items: Vec<RepoEntry> },
    /// Each entry's `linked` field tells whether it's currently tied to the ticket.
    TicketProjects { items: Vec<TicketProjectEntry> },
    ConfluenceSpaces {
        items: Vec<crate::confluence_api::ConfluenceSpace>,
        /// True when served from SQLite cache without a live API call.
        from_cache: bool,
    },
    ConfluencePages {
        items: Vec<crate::confluence_api::ConfluencePage>,
        from_cache: bool,
    },
    Ok,
    Err { message: String },
    /// `from_cache` = true means results came from ticket assignees (users table empty).
    Users { items: Vec<UserInfo>, from_cache: bool },
    Teams { items: Vec<TeamEntry> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamEntry {
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StartWorkReply {
    GitWorktree {
        branch: String,
        path: std::path::PathBuf,
        created_branch: bool,
        attached_existing_worktree: bool,
    },
    SvnExport { value: String },
    NoScm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationItem {
    pub kind: String,
    pub ticket_key: String,
    pub message: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectStatus {
    pub path: PathBuf,
    pub nickname: Option<String>,
    pub available: bool,
    /// "git", "svn", or "missing".
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketProjectEntry {
    pub project: ProjectStatus,
    /// Whether the project is linked at all (state != "rejected").
    pub linked: bool,
    /// "confirmed" | "suggested" | "none" (none = not linked / never suggested).
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub started_at: String,
    pub last_poll_at: Option<String>,
    pub cached_tickets: usize,
}

pub async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, payload: &[u8]) -> Result<()> {
    let len = payload.len();
    if len as u32 > MAX_FRAME {
        return Err(anyhow!("frame too large ({len} bytes)"));
    }
    w.write_all(&(len as u32).to_be_bytes()).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await.context("reading frame length")?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(anyhow!("frame too large ({len} bytes)"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await.context("reading frame body")?;
    Ok(buf)
}

pub async fn send_request(stream: &mut UnixStream, req: &Request) -> Result<Response> {
    let raw = serde_json::to_vec(req)?;
    write_frame(stream, &raw).await?;
    let resp_raw = read_frame(stream).await?;
    let resp: Response = serde_json::from_slice(&resp_raw)?;
    Ok(resp)
}

pub async fn connect() -> Result<UnixStream> {
    let path = crate::paths::socket_path()?;
    UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to daemon at {}", path.display()))
}
