use anyhow::{Context, Result};
use tracing_subscriber::fmt::writer::MakeWriterExt;
use clap::Parser;
use jui_core::cache::Cache;
use jui_core::confluence_api::ConfluenceApi;
use jui_core::config::GlobalConfig;
use jui_core::ipc::{
    read_frame, write_frame, DaemonStatus, NotificationItem, ProjectStatus, Request, Response,
    StartWorkReply, TicketProjectEntry,
};
use jui_core::jira::JiraCli;
use jui_core::jira_api::JiraApi;
use jui_core::paths;
use jui_core::scm;
use jui_core::ticket::Ticket;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "jui-daemon", about = "Jui background daemon")]
struct Args {
    /// Run in foreground (don't daemonize). Logging goes to stderr.
    #[arg(long, default_value_t = true)]
    foreground: bool,
}

struct State {
    config: GlobalConfig,
    jira: JiraCli,
    cache: Mutex<Cache>,
    started_at: chrono::DateTime<chrono::Utc>,
    last_poll: RwLock<Option<chrono::DateTime<chrono::Utc>>>,
    pending: Mutex<Vec<NotificationItem>>,
    /// Queue of ticket keys awaiting a claude project-suggestion.
    suggest_tx: tokio::sync::mpsc::Sender<String>,
    /// Queue of ticket keys awaiting a claude-code implementation generation.
    impl_tx: tokio::sync::mpsc::Sender<String>,
}

impl State {
    fn new(
        config: GlobalConfig,
        suggest_tx: tokio::sync::mpsc::Sender<String>,
        impl_tx: tokio::sync::mpsc::Sender<String>,
    ) -> Result<Self> {
        let jira = JiraCli::new(config.jira.binary.clone());
        let cache = Cache::open(&paths::cache_db()?)?;
        Ok(Self {
            config,
            jira,
            cache: Mutex::new(cache),
            started_at: chrono::Utc::now(),
            last_poll: RwLock::new(None),
            pending: Mutex::new(Vec::new()),
            suggest_tx,
            impl_tx,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Append to /tmp/jui.log alongside stderr. The non-blocking writer needs its
    // _guard to live for the program's lifetime to flush on shutdown.
    let file = tracing_appender::rolling::never("/tmp", "jui.log");
    let (file_writer, _guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,jui_daemon=debug,jui_core=debug")),
        )
        .with_writer(file_writer.and(std::io::stderr))
        .with_ansi(false)
        .init();
    // Keep the guard alive for the lifetime of the process.
    Box::leak(Box::new(_guard));
    let _args = Args::parse();
    paths::ensure_dirs()?;

    let socket = paths::socket_path()?;
    if socket.exists() {
        // Try to connect; if a live daemon answers, refuse to start. Otherwise stale — remove.
        if (UnixStream::connect(&socket).await).is_ok() {
            anyhow::bail!("a daemon is already running at {}", socket.display());
        }
        std::fs::remove_file(&socket).ok();
    }

    let pid_path = paths::pid_file()?;
    std::fs::write(&pid_path, std::process::id().to_string())?;

    let cfg = GlobalConfig::load().unwrap_or_default();
    sanity_check_projects(&cfg);
    let (suggest_tx, suggest_rx) = tokio::sync::mpsc::channel::<String>(256);
    let (impl_tx, impl_rx) = tokio::sync::mpsc::channel::<String>(256);
    let state = Arc::new(State::new(cfg, suggest_tx.clone(), impl_tx.clone())?);

    // Suggestion worker — single consumer so claude is invoked sequentially.
    {
        let state = state.clone();
        tokio::spawn(async move { suggestion_worker(state, suggest_rx).await });
    }
    // Implementation-generation worker — also single-consumer.
    {
        let state = state.clone();
        tokio::spawn(async move { implementation_worker(state, impl_rx).await });
    }
    // On startup, enqueue tickets needing project suggestions and tickets that have
    // a project but no implementation yet. Also seed the user cache.
    {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Ok(keys) = state.cache.lock().await.tickets_needing_suggestion() {
                for k in keys {
                    let _ = state.suggest_tx.send(k).await;
                }
            }
            if let Ok(keys) = state.cache.lock().await.tickets_needing_implementation() {
                for k in keys {
                    let _ = state.impl_tx.send(k).await;
                }
            }
            // Seed user cache. Best-effort — log and move on if it fails.
            if let Ok(api) = JiraApi::from_jira_cli_config() {
                match api.list_all_users().await {
                    Ok(users) => {
                        info!(count = users.len(), "fetched Jira users");
                        if let Err(e) = state.cache.lock().await.upsert_users(&users) {
                            warn!("storing users failed: {e:#}");
                        }
                    }
                    Err(e) => warn!("fetching users failed (non-fatal): {e:#}"),
                }
            }
        });
    }

    // Confluence cache warmup — runs after a short delay to not block startup.
    {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            warm_confluence_cache(&state).await;
        });
    }

    // Background poller for "my work" + notifications.
    {
        let state = state.clone();
        tokio::spawn(async move { poll_loop(state).await });
    }

    // Background warmup of ticket parent chains for Tree mode (every hour).
    {
        let state = state.clone();
        tokio::spawn(async move { ancestor_warmup_loop(state).await });
    }

    // Tmux status writer.
    {
        let state = state.clone();
        tokio::spawn(async move { tmux_status_loop(state).await });
    }

    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("binding {}", socket.display()))?;
    info!(path = %socket.display(), "daemon listening");

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, _addr) = accept?;
                let state = state.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, state, shutdown_tx).await {
                        warn!("client error: {e:#}");
                    }
                });
            }
            _ = shutdown_rx.recv() => {
                info!("shutdown requested");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c, shutting down");
                break;
            }
        }
    }

    std::fs::remove_file(&socket).ok();
    std::fs::remove_file(&pid_path).ok();
    Ok(())
}

/// Log warnings for any configured project paths that are missing or no longer SCM repos.
fn sanity_check_projects(cfg: &GlobalConfig) {
    for p in &cfg.projects {
        let kind = project_kind(&p.path);
        if kind == "missing" {
            warn!(path = %p.path.display(), "configured project is unavailable");
        } else {
            info!(path = %p.path.display(), kind = kind, "project ready");
        }
    }
}

fn project_kind(path: &Path) -> &'static str {
    if !path.exists() { return "missing"; }
    if path.join(".git").exists() { return "git"; }
    if path.join(".svn").exists() { return "svn"; }
    "missing"
}

async fn handle_client(
    mut stream: UnixStream,
    state: Arc<State>,
    shutdown: tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    let raw = read_frame(&mut stream).await?;
    let req: Request = serde_json::from_slice(&raw)?;
    let resp = match dispatch(req, state.clone(), &shutdown).await {
        Ok(r) => r,
        Err(e) => Response::Err { message: format!("{e:#}") },
    };
    let body = serde_json::to_vec(&resp)?;
    write_frame(&mut stream, &body).await?;
    Ok(())
}

async fn dispatch(
    req: Request,
    state: Arc<State>,
    shutdown: &tokio::sync::mpsc::Sender<()>,
) -> Result<Response> {
    match req {
        Request::Ping => Ok(Response::Pong),

        Request::ListTickets { jql, limit } => {
            let custom_jql = jql.is_some();
            let q = jql.unwrap_or_else(|| state.config.jira.my_jql.clone());
            let mut tickets = match state.jira.search(&q, limit).await {
                Ok(mut tickets) => {
                    if let Err(e) = state.jira.resolve_subtasks(&mut tickets, 5).await {
                        warn!("subtask resolution failed (continuing): {e:#}");
                    }
                    if let Err(e) = state.jira.resolve_parents(&mut tickets).await {
                        warn!("parent resolution failed (continuing): {e:#}");
                    }
                    let mut cache = state.cache.lock().await;
                    cache.upsert_tickets(&tickets).ok();
                    tickets
                }
                Err(e) => {
                    if custom_jql {
                        return Ok(Response::Err { message: format!("search failed: {e:#}") });
                    }
                    warn!("live search failed, falling back to cache: {e:#}");
                    let cache = state.cache.lock().await;
                    cache.list_tickets(limit as i64)?
                }
            };
            // Decorate with confirmed local-project links for sort/display.
            let keys: Vec<String> = tickets.iter().map(|t| t.key.clone()).collect();
            let map = state.cache.lock().await.confirmed_projects_for_tickets(&keys)?;
            for t in &mut tickets {
                if let Some(paths) = map.get(&t.key) {
                    t.linked_projects = paths.clone();
                }
            }
            Ok(Response::Tickets { items: tickets })
        }

        Request::GetTicketsWithAncestors { keys } => {
            let cache = state.cache.lock().await;
            let items = cache.tickets_with_ancestors(&keys)?;
            Ok(Response::Tickets { items })
        }

        Request::GetTicket { key } => {
            match state.jira.view(&key).await {
                Ok(t) => {
                    let mut cache = state.cache.lock().await;
                    cache.upsert_tickets(std::slice::from_ref(&t)).ok();
                    Ok(Response::Ticket { ticket: t })
                }
                Err(_) => {
                    let cache = state.cache.lock().await;
                    match cache.get_ticket(&key)? {
                        Some(t) => Ok(Response::Ticket { ticket: t }),
                        None => Ok(Response::Err { message: format!("no such ticket {key}") }),
                    }
                }
            }
        }

        Request::Refresh { jql } => {
            let q = jql.unwrap_or_else(|| state.config.jira.my_jql.clone());
            let mut tickets = state.jira.search(&q, 100).await?;
            if let Err(e) = state.jira.resolve_subtasks(&mut tickets, 5).await {
                warn!("subtask resolution failed (continuing): {e:#}");
            }
            if let Err(e) = state.jira.resolve_parents(&mut tickets).await {
                warn!("parent resolution failed (continuing): {e:#}");
            }
            let mut cache = state.cache.lock().await;
            cache.upsert_tickets(&tickets)?;
            Ok(Response::Ok)
        }

        Request::StartWork { key, cwd } => {
            let ticket = match state.jira.view(&key).await {
                Ok(t) => t,
                Err(_) => state
                    .cache
                    .lock()
                    .await
                    .get_ticket(&key)?
                    .ok_or_else(|| anyhow::anyhow!("no such ticket {key}"))?,
            };
            let slug = ticket.branch_slug();
            let repo = scm::detect(&cwd);
            let outcome = scm::start_work(&repo, &key, &slug)?;
            let reply = match outcome {
                scm::StartWorkOutcome::GitWorktree {
                    branch,
                    path,
                    created_branch,
                    attached_existing_worktree,
                } => StartWorkReply::GitWorktree {
                    branch,
                    path,
                    created_branch,
                    attached_existing_worktree,
                },
                scm::StartWorkOutcome::SvnExport { value } => StartWorkReply::SvnExport { value },
                scm::StartWorkOutcome::NoScm => StartWorkReply::NoScm,
            };
            Ok(Response::StartWork { reply })
        }

        Request::AddComment { key, body } => {
            state.jira.add_comment(&key, &body).await?;
            Ok(Response::Ok)
        }

        Request::ListComments { key } => {
            let items = state.jira.comments(&key).await?;
            Ok(Response::Comments { items })
        }

        Request::DeleteComment { key, comment_id } => {
            let api = JiraApi::from_jira_cli_config()?;
            api.delete_comment(&key, &comment_id).await?;
            Ok(Response::Ok)
        }

        Request::Myself => {
            let api = JiraApi::from_jira_cli_config()?;
            let info = api.myself().await?;
            Ok(Response::Myself { info })
        }

        Request::ListProjects => {
            // Re-read config every call so external edits show up.
            let cfg = GlobalConfig::load().unwrap_or_default();
            let items: Vec<ProjectStatus> = cfg
                .projects
                .iter()
                .map(|p| {
                    let kind = project_kind(&p.path);
                    ProjectStatus {
                        path: p.path.clone(),
                        nickname: p.nickname.clone(),
                        available: kind != "missing",
                        kind: kind.into(),
                    }
                })
                .collect();
            Ok(Response::Projects { items })
        }

        Request::AddProject { path, nickname } => {
            let mut cfg = GlobalConfig::load().unwrap_or_default();
            if !cfg.add_project(path, nickname) {
                return Ok(Response::Err { message: "project already in config".into() });
            }
            cfg.save()?;
            // Re-suggest for every ticket the user hasn't already confirmed a project on.
            // We wipe stale suggestions and the synthetic no-match row first so the
            // worker doesn't short-circuit, then enqueue the keys.
            let keys = state.cache.lock().await.tickets_without_confirmed()?;
            if !keys.is_empty() {
                state.cache.lock().await.clear_unconfirmed(&keys)?;
                let queue = keys.len();
                for k in keys {
                    let _ = state.suggest_tx.send(k).await;
                }
                info!(re_suggesting = queue, "queued re-suggestion after project add");
            }
            Ok(Response::Ok)
        }

        Request::RemoveProject { path } => {
            let mut cfg = GlobalConfig::load().unwrap_or_default();
            if cfg.remove_project(&path) {
                cfg.save()?;
                Ok(Response::Ok)
            } else {
                Ok(Response::Err { message: "project not in config".into() })
            }
        }

        Request::LinkProject { ticket_key, project_path } => {
            let canonical = std::fs::canonicalize(&project_path).unwrap_or(project_path);
            state.cache.lock().await.link_project(&ticket_key, &canonical.display().to_string())?;
            let _ = state.impl_tx.send(ticket_key).await;
            Ok(Response::Ok)
        }

        Request::UnlinkProject { ticket_key, project_path } => {
            let canonical = std::fs::canonicalize(&project_path).unwrap_or(project_path);
            state.cache.lock().await.unlink_project(&ticket_key, &canonical.display().to_string())?;
            Ok(Response::Ok)
        }

        Request::ListTicketProjects { ticket_key } => {
            let cfg = GlobalConfig::load().unwrap_or_default();
            use std::collections::HashMap;
            let states: HashMap<String, String> = state
                .cache
                .lock()
                .await
                .projects_for_ticket(&ticket_key)?
                .into_iter()
                .collect();
            let mut items: Vec<TicketProjectEntry> = cfg
                .projects
                .iter()
                .map(|p| {
                    let kind = project_kind(&p.path);
                    let path_str = p.path.display().to_string();
                    let state = states.get(&path_str).cloned().unwrap_or_else(|| "none".into());
                    TicketProjectEntry {
                        linked: state == "confirmed" || state == "suggested",
                        state,
                        project: ProjectStatus {
                            path: p.path.clone(),
                            nickname: p.nickname.clone(),
                            available: kind != "missing",
                            kind: kind.into(),
                        },
                    }
                })
                .collect();
            // Synthetic "no clear match" row, present whenever the suggester ran but
            // didn't pick a project. Surfaced so the user can see claude was attempted.
            if states.get("(none)").map(|s| s == "no_match").unwrap_or(false) {
                items.push(TicketProjectEntry {
                    linked: true,
                    state: "no_match".into(),
                    project: ProjectStatus {
                        path: PathBuf::from("(none)"),
                        nickname: None,
                        available: false,
                        kind: "synthetic".into(),
                    },
                });
            }
            Ok(Response::TicketProjects { items })
        }

        Request::ConfirmSuggestion { ticket_key, project_path } => {
            let canonical = std::fs::canonicalize(&project_path).unwrap_or(project_path);
            state.cache.lock().await.confirm_suggestion(&ticket_key, &canonical.display().to_string())?;
            let _ = state.impl_tx.send(ticket_key).await;
            Ok(Response::Ok)
        }

        Request::RejectSuggestion { ticket_key, project_path } => {
            let canonical = std::fs::canonicalize(&project_path).unwrap_or(project_path);
            state.cache.lock().await.reject_suggestion(&ticket_key, &canonical.display().to_string())?;
            Ok(Response::Ok)
        }

        Request::SuggestProject { ticket_key } => {
            let _ = state.suggest_tx.send(ticket_key).await;
            Ok(Response::Ok)
        }

        Request::GetImplementation { ticket_key } => {
            match state.cache.lock().await.get_implementation(&ticket_key)? {
                Some((markdown, project_paths, updated_at)) => Ok(Response::Implementation {
                    markdown,
                    project_paths,
                    updated_at,
                }),
                None => Ok(Response::Err { message: "no implementation cached yet".into() }),
            }
        }

        Request::GenerateImplementation { ticket_key } => {
            let _ = state.impl_tx.send(ticket_key).await;
            Ok(Response::Queued)
        }

        Request::GetClaudeSession { ticket_key } => {
            let id = state.cache.lock().await.get_claude_session(&ticket_key)?;
            Ok(Response::ClaudeSession { session_id: id })
        }

        Request::SaveClaudeSession { ticket_key, session_id } => {
            state.cache.lock().await.set_claude_session(&ticket_key, &session_id)?;
            Ok(Response::Ok)
        }

        Request::ScanRepos { root, max_depth } => {
            let root = if root.as_os_str().is_empty() {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            } else {
                root
            };
            // Run in a blocking thread because file walks can be slow on big trees.
            let depth = max_depth.max(1) as usize;
            let items = tokio::task::spawn_blocking(move || jui_core::scm::find_repos(&root, depth))
                .await
                .map_err(|e| anyhow::anyhow!("scan task failed: {e}"))?;
            Ok(Response::Repos { items })
        }

        Request::Transition { key, to } => {
            state.jira.transition(&key, &to).await?;
            Ok(Response::Ok)
        }

        Request::ListTransitions { key } => {
            let api = JiraApi::from_jira_cli_config()?;
            let items = api.list_transitions(&key).await?;
            Ok(Response::Transitions { items })
        }

        Request::CreateTicket { project, issue_type, summary, body, parent } => {
            let key = state
                .jira
                .create(&project, &issue_type, &summary, body.as_deref(), parent.as_deref())
                .await?;
            Ok(Response::Created { key })
        }

        Request::DeleteTicket { key } => {
            // jira-cli's `issue delete` is interactive and rejects --no-input, so
            // use the REST endpoint directly.
            let api = JiraApi::from_jira_cli_config()?;
            api.delete_issue(&key).await?;
            let _ = state.cache.lock().await.delete_ticket(&key);
            tracing::info!(%key, "deleted ticket");
            Ok(Response::Ok)
        }

        Request::ArchiveTicket { key } => {
            // List the ticket's transitions, pick the first archive-like one, fire it.
            let api = JiraApi::from_jira_cli_config()?;
            let items = api.list_transitions(&key).await?;
            let prefs = ["archive", "won't do", "wont do", "cancelled", "canceled", "closed", "done"];
            let target = prefs.iter().find_map(|want| {
                items.iter().find(|tr| {
                    tr.to_status
                        .as_deref()
                        .map(|s| s.eq_ignore_ascii_case(want))
                        .unwrap_or(false)
                        || tr.name.to_ascii_lowercase().contains(want)
                })
            });
            let Some(tr) = target else {
                let names: Vec<String> = items.iter().map(|t| {
                    format!("{} → {}", t.name, t.to_status.clone().unwrap_or_default())
                }).collect();
                return Ok(Response::Err {
                    message: format!(
                        "no archive-like transition for {key}. available: {}",
                        names.join(", ")
                    ),
                });
            };
            state.jira.transition(&key, &tr.name).await?;
            tracing::info!(%key, transition = %tr.name, "archived ticket");
            // Drop from local cache; refresh will re-add if it still appears in JQL.
            let _ = state.cache.lock().await.delete_ticket(&key);
            Ok(Response::Ok)
        }

        Request::AssignTicket { key, assignee } => {
            // The picker hands us a Jira account id, which jira-cli's `issue assign`
            // rejects on Cloud. Use the REST endpoint instead. Falls back to jira-cli
            // if the value doesn't look like an account id (e.g. user typed an email).
            let looks_like_account_id = assignee.contains(':') || assignee.len() >= 24;
            if looks_like_account_id {
                let api = JiraApi::from_jira_cli_config()?;
                api.set_assignee(&key, &assignee).await?;
            } else {
                state.jira.assign(&key, &assignee).await?;
            }
            Ok(Response::Ok)
        }

        Request::SetReviewer { key, assignee_id } => {
            let api = JiraApi::from_jira_cli_config()?;
            api.set_reviewer(&key, &assignee_id, &state.config.jira.reviewer_customfield).await?;
            Ok(Response::Ok)
        }

        Request::EditSummary { key, summary } => {
            state.jira.edit_summary(&key, &summary).await?;
            Ok(Response::Ok)
        }

        Request::EditPriority { key, priority } => {
            // Priority via REST: jira-cli's `-y` flag is rejected by some Jira
            // instances. The REST PUT endpoint reliably accepts {"name": ...}.
            let api = JiraApi::from_jira_cli_config()?;
            api.set_priority(&key, &priority).await?;
            Ok(Response::Ok)
        }

        Request::ListPriorities => {
            let api = JiraApi::from_jira_cli_config()?;
            let items = api.priorities().await?;
            Ok(Response::Priorities { items })
        }

        Request::SetEstimate { key, original, remaining } => {
            let api = JiraApi::from_jira_cli_config()?;
            api.set_estimate(&key, original.as_deref(), remaining.as_deref()).await?;
            Ok(Response::Ok)
        }

        Request::LogWork { key, time_spent, comment, new_estimate } => {
            state.jira.worklog_add(&key, &time_spent, comment.as_deref(), new_estimate.as_deref()).await?;
            Ok(Response::Ok)
        }

        Request::PendingNotifications => {
            let mut p = state.pending.lock().await;
            let out = std::mem::take(&mut *p);
            Ok(Response::Notifications { items: out })
        }

        Request::Status => {
            let cache = state.cache.lock().await;
            let count = cache.known_keys().map(|v| v.len()).unwrap_or(0);
            Ok(Response::Status { status: DaemonStatus {
                pid: std::process::id(),
                started_at: state.started_at.to_rfc3339(),
                last_poll_at: state.last_poll.read().await.map(|t| t.to_rfc3339()),
                cached_tickets: count,
            } })
        }

        Request::Shutdown => {
            let _ = shutdown.send(()).await;
            Ok(Response::Ok)
        }

        Request::SearchUsers { query } => {
            let (items, from_cache) = state.cache.lock().await.search_users(&query)?;
            Ok(Response::Users { items, from_cache })
        }

        Request::SaveTeam { name, members } => {
            state.cache.lock().await.save_team(&name, &members)?;
            Ok(Response::Ok)
        }

        Request::ListTeams => {
            let raw = state.cache.lock().await.list_teams()?;
            let items = raw.into_iter().map(|(name, members)| jui_core::ipc::TeamEntry { name, members }).collect();
            Ok(Response::Teams { items })
        }

        Request::DeleteTeam { name } => {
            state.cache.lock().await.delete_team(&name)?;
            Ok(Response::Ok)
        }

        Request::ConfluenceListSpaces => {
            const STALE_SECS: u64 = 900; // 15 minutes
            let cached = state.cache.lock().await.get_confluence_spaces()?;
            let age = state.cache.lock().await.confluence_spaces_age_secs()?;

            if cached.is_empty() {
                // Cache cold — must fetch now (user waits once).
                let api = ConfluenceApi::from_jira_config()?;
                let spaces = api.list_spaces().await?;
                state.cache.lock().await.upsert_confluence_spaces(&spaces)?;
                return Ok(Response::ConfluenceSpaces { items: spaces, from_cache: false });
            }

            // Serve cache immediately; refresh in background if stale.
            if age.map(|a| a > STALE_SECS).unwrap_or(true) {
                let state2 = state.clone();
                tokio::spawn(async move {
                    if let Ok(api) = ConfluenceApi::from_jira_config() {
                        if let Ok(spaces) = api.list_spaces().await {
                            let _ = state2.cache.lock().await.upsert_confluence_spaces(&spaces);
                        }
                    }
                });
            }
            Ok(Response::ConfluenceSpaces { items: cached, from_cache: true })
        }

        Request::ConfluenceListPages { space_key, parent_id } => {
            const STALE_SECS: u64 = 900;
            let pid = parent_id.as_deref();
            let cached = state.cache.lock().await.get_confluence_pages(&space_key, pid)?;
            let age = state.cache.lock().await.confluence_pages_age_secs(&space_key, pid)?;

            if cached.is_empty() {
                let api = ConfluenceApi::from_jira_config()?;
                let pages = match pid {
                    Some(id) => api.get_children(id).await?,
                    None => api.list_pages(&space_key).await?,
                };
                state.cache.lock().await.upsert_confluence_pages(&space_key, pid, &pages)?;
                return Ok(Response::ConfluencePages { items: pages, from_cache: false });
            }

            if age.map(|a| a > STALE_SECS).unwrap_or(true) {
                let state2 = state.clone();
                let sk = space_key.clone();
                let pi = parent_id.clone();
                tokio::spawn(async move {
                    if let Ok(api) = ConfluenceApi::from_jira_config() {
                        let pid2 = pi.as_deref();
                        let result = match pid2 {
                            Some(id) => api.get_children(id).await,
                            None => api.list_pages(&sk).await,
                        };
                        if let Ok(pages) = result {
                            let _ = state2.cache.lock().await.upsert_confluence_pages(&sk, pid2, &pages);
                        }
                    }
                });
            }
            Ok(Response::ConfluencePages { items: cached, from_cache: true })
        }
    }
}

async fn warm_confluence_cache(state: &State) {
    let api = match ConfluenceApi::from_jira_config() {
        Ok(a) => a,
        Err(e) => { warn!("confluence config missing, skipping warmup: {e:#}"); return; }
    };
    let spaces = match api.list_spaces().await {
        Ok(s) => s,
        Err(e) => { warn!("confluence spaces warmup failed: {e:#}"); return; }
    };
    if let Err(e) = state.cache.lock().await.upsert_confluence_spaces(&spaces) {
        warn!("storing confluence spaces: {e:#}");
        return;
    }
    info!(count = spaces.len(), "confluence spaces cached");
    // Recursive walk: for each space, fetch root pages → for each, fetch children
    // → repeat until depth cap. Uses a BFS so we can rate-limit between API calls.
    const MAX_DEPTH: usize = 4;
    for space in &spaces {
        let roots = match api.list_pages(&space.key).await {
            Ok(p) => p,
            Err(e) => {
                warn!(space = %space.key, "confluence pages warmup failed: {e:#}");
                continue;
            }
        };
        if let Err(e) = state.cache.lock().await.upsert_confluence_pages(&space.key, None, &roots) {
            warn!(space = %space.key, "storing root pages: {e:#}");
            continue;
        }
        let mut total = roots.len();
        let mut frontier: Vec<(String, usize)> =
            roots.iter().filter(|p| p.has_children).map(|p| (p.id.clone(), 1)).collect();
        while let Some((pid, depth)) = frontier.pop() {
            if depth > MAX_DEPTH { continue; }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            match api.get_children(&pid).await {
                Ok(children) => {
                    if let Err(e) = state.cache.lock().await
                        .upsert_confluence_pages(&space.key, Some(&pid), &children)
                    {
                        warn!(parent = %pid, "storing children: {e:#}");
                        continue;
                    }
                    total += children.len();
                    for c in &children {
                        if c.has_children { frontier.push((c.id.clone(), depth + 1)); }
                    }
                }
                Err(e) => warn!(parent = %pid, "fetching children failed: {e:#}"),
            }
        }
        info!(space = %space.key, total, "confluence pages cached (recursive)");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Walk parent_key chains for every cached ticket and store the ancestors in cache,
/// so that Tree mode can read everything from SQLite without hitting Jira live.
async fn warm_ticket_ancestors(state: &State) {
    use std::collections::HashSet;
    // Pull every cached ticket and ALSO replace its row with the rich `view`
    // payload (description, subtasks, time tracking, custom fields) so detail-open
    // is a cache hit. Then walk parent chains for ancestors.
    let initial = match state.cache.lock().await.list_tickets(10_000) {
        Ok(t) => t,
        Err(e) => { warn!("ancestor warmup: list_tickets failed: {e:#}"); return; }
    };
    let initial_keys: Vec<String> = initial.iter().map(|t| t.key.clone()).collect();
    let mut seen: HashSet<String> = initial_keys.iter().cloned().collect();
    let mut queue: Vec<String> = initial_keys.clone();
    let mut fetched = 0usize;
    let mut ancestors_added = 0usize;
    while let Some(key) = queue.pop() {
        let was_initial = initial_keys.iter().any(|k| k == &key);
        match state.jira.view(&key).await {
            Ok(t) => {
                let next = t.parent_key.clone();
                if let Err(e) = state.cache.lock().await.upsert_tickets(std::slice::from_ref(&t)) {
                    warn!(key = %key, "cache upsert failed: {e:#}");
                }
                fetched += 1;
                if !was_initial { ancestors_added += 1; }
                if let Some(n) = next {
                    if seen.insert(n.clone()) { queue.push(n); }
                }
            }
            Err(e) => warn!(key = %key, "view failed: {e:#}"),
        }
        // Be polite to the Jira API.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    info!(fetched, ancestors_added, "ticket warmup complete");
}

async fn ancestor_warmup_loop(state: Arc<State>) {
    // Initial run after a small delay so login / refresh have happened.
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    warm_ticket_ancestors(&state).await;
    let mut iv = interval(Duration::from_secs(3600)); // 1 hour
    iv.tick().await; // skip the immediate tick — we just ran above.
    loop {
        iv.tick().await;
        warm_ticket_ancestors(&state).await;
    }
}

async fn poll_loop(state: Arc<State>) {
    let mut iv = interval(Duration::from_secs(state.config.poll.interval_secs.max(15)));
    iv.tick().await; // first tick fires immediately
    loop {
        if let Err(e) = poll_once(&state).await {
            warn!("poll failed: {e:#}");
        }
        iv.tick().await;
    }
}

async fn poll_once(state: &State) -> Result<()> {
    let known: HashSet<String> = {
        let cache = state.cache.lock().await;
        cache.known_keys()?.into_iter().collect()
    };
    let mut tickets = state.jira.search(&state.config.jira.my_jql, 100).await?;
    if let Err(e) = state.jira.resolve_subtasks(&mut tickets, 5).await {
        warn!("subtask resolution failed in poll (continuing): {e:#}");
    }
    if let Err(e) = state.jira.resolve_parents(&mut tickets).await {
        warn!("parent resolution failed in poll (continuing): {e:#}");
    }
    let mut new_assignments: Vec<&Ticket> = Vec::new();
    for t in &tickets {
        if !known.contains(&t.key) {
            new_assignments.push(t);
        }
    }
    {
        let mut cache = state.cache.lock().await;
        cache.upsert_tickets(&tickets)?;
        for t in &new_assignments {
            cache
                .record_notification("assignment", &t.key, &format!("Assigned: {}", t.summary))
                .ok();
        }
    }
    {
        let mut p = state.pending.lock().await;
        for t in &new_assignments {
            p.push(NotificationItem {
                kind: "assignment".into(),
                ticket_key: t.key.clone(),
                message: format!("Assigned: {}", t.summary),
                created_at: chrono::Utc::now().to_rfc3339(),
            });
        }
    }
    if state.config.notifications.on_assignment {
        for t in &new_assignments {
            send_desktop(&format!("Jira: assigned {}", t.key), &t.summary);
        }
    }
    // Enqueue every newly-assigned ticket for claude suggestion. The worker no-ops on
    // tickets that already have any row in ticket_projects, so re-enqueueing is safe.
    for t in &new_assignments {
        let _ = state.suggest_tx.send(t.key.clone()).await;
    }
    *state.last_poll.write().await = Some(chrono::Utc::now());
    info!(count = tickets.len(), new = new_assignments.len(), "poll complete");
    Ok(())
}

fn send_desktop(summary: &str, body: &str) {
    if let Err(e) = notify_rust::Notification::new().summary(summary).body(body).show() {
        warn!("desktop notification failed: {e}");
    }
}

async fn implementation_worker(
    state: Arc<State>,
    mut rx: tokio::sync::mpsc::Receiver<String>,
) {
    while let Some(ticket_key) = rx.recv().await {
        if let Err(e) = process_implementation(&state, &ticket_key).await {
            warn!(ticket = %ticket_key, "implementation generation failed: {e:#}");
        }
    }
}

async fn process_implementation(state: &State, ticket_key: &str) -> Result<()> {
    // Pull the ticket fresh — fall back to cache.
    let ticket = match state.jira.view(ticket_key).await {
        Ok(t) => t,
        Err(_) => match state.cache.lock().await.get_ticket(ticket_key)? {
            Some(t) => t,
            None => return Ok(()),
        },
    };
    // Gather projects: confirmed first, then suggested. Cap at 3.
    let rows = state.cache.lock().await.projects_for_ticket(ticket_key)?;
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for state_filter in ["confirmed", "suggested"] {
        for (path_s, st) in &rows {
            if st == state_filter && seen.insert(path_s.clone()) && path_s != "(none)" {
                let pb = std::path::PathBuf::from(path_s);
                if pb.exists() {
                    paths.push(pb);
                }
            }
            if paths.len() >= 3 { break; }
        }
        if paths.len() >= 3 { break; }
    }
    if paths.is_empty() {
        return Ok(());
    }
    info!(ticket = %ticket_key, projects = paths.len(), "asking claude for implementation");
    let markdown = jui_core::claude::propose_implementation(&ticket, &paths).await?;
    let project_strs: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    state.cache.lock().await.set_implementation(ticket_key, &markdown, &project_strs)?;
    info!(ticket = %ticket_key, "implementation saved");
    Ok(())
}

async fn suggestion_worker(
    state: Arc<State>,
    mut rx: tokio::sync::mpsc::Receiver<String>,
) {
    while let Some(ticket_key) = rx.recv().await {
        if let Err(e) = process_suggestion(&state, &ticket_key).await {
            warn!(ticket = %ticket_key, "suggestion failed: {e:#}");
        }
    }
}

async fn process_suggestion(state: &State, ticket_key: &str) -> Result<()> {
    // Skip tickets that already have any row in ticket_projects (confirmed, suggested,
    // or rejected). Each value short-circuits a re-suggestion.
    let existing = state.cache.lock().await.projects_for_ticket(ticket_key)?;
    if !existing.is_empty() {
        return Ok(());
    }
    let cfg = jui_core::config::GlobalConfig::load().unwrap_or_default();
    if cfg.projects.is_empty() {
        return Ok(());
    }
    // Fetch fresh ticket details — try live, fall back to cache.
    let ticket = match state.jira.view(ticket_key).await {
        Ok(t) => t,
        Err(_) => match state.cache.lock().await.get_ticket(ticket_key)? {
            Some(t) => t,
            None => return Ok(()),
        },
    };
    info!(ticket = %ticket_key, "asking claude for project suggestions");
    let picks = jui_core::claude::suggest_projects(&ticket, &cfg.projects).await?;
    if picks.is_empty() {
        // Surfaced in the UI as a "no clear match" entry — user can dismiss it (which
        // promotes it to 'rejected' and silences future surfacing).
        state.cache.lock().await.record_no_match(ticket_key, "claude")?;
        info!(ticket = %ticket_key, "claude found no clear match");
    } else {
        let cache = state.cache.lock().await;
        for path in &picks {
            let path_s = path.display().to_string();
            cache.add_suggestion(ticket_key, &path_s, "claude")?;
        }
        info!(
            ticket = %ticket_key,
            picks = picks.len(),
            "claude suggested {} project(s)", picks.len()
        );
    }
    Ok(())
}

async fn tmux_status_loop(state: Arc<State>) {
    let path = match paths::status_file() {
        Ok(p) => p,
        Err(e) => {
            error!("no status path: {e}");
            return;
        }
    };
    let mut iv = interval(Duration::from_secs(5));
    loop {
        iv.tick().await;
        let count = state.cache.lock().await.known_keys().map(|v| v.len()).unwrap_or(0);
        let pending = state.pending.lock().await.len();
        let s = if pending > 0 {
            format!("[jui {} | * {}]", count, pending)
        } else {
            format!("[jui {}]", count)
        };
        let _ = std::fs::write(&path, s);
    }
}

#[allow(dead_code)]
fn _unused(_p: PathBuf) {}
