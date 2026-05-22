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

    // Comment + mentions warmup. First run delayed so login / refresh have
    // a chance to populate the tickets table; both then re-run on the same
    // cadence as poll_loop.
    {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            warm_comments(&state).await;
            refresh_my_mentions(&state).await;
            let _ = refresh_github_mentions(&state).await;
        });
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
            // Prefer the ticket's first confirmed linked project as the SCM
            // anchor — guarantees the worktree lands next to the repo the
            // ticket actually targets, even when the user launched jui from
            // outside that repo. Falls back to the caller-supplied cwd
            // (typically the TUI's working dir) when no linked project is
            // recorded.
            let scm_anchor = state
                .cache
                .lock()
                .await
                .linked_paths(&key)?
                .into_iter()
                .next()
                .unwrap_or(cwd);
            let repo = scm::detect(&scm_anchor);
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
            // Async refresh of the affected ticket's comments + view.
            let s2 = state.clone();
            let k2 = key.clone();
            tokio::spawn(async move { refresh_ticket_after_mutation(&s2, &k2).await; });
            Ok(Response::Ok)
        }

        Request::ListComments { key } => {
            // Cache-first, but synchronously refresh if the row is older than
            // 60s OR completely missing — the user expects to see new comments
            // when they open a ticket, not on the next refresh tick.
            const FRESH: i64 = 60;
            let cached = state.cache.lock().await.get_comments(&key)?;
            let age = state.cache.lock().await.comments_age_secs(&key)?;
            let stale = age.map(|a| a > FRESH).unwrap_or(true);
            if stale {
                match state.jira.comments(&key).await {
                    Ok(items) => {
                        let _ = state.cache.lock().await.upsert_comments(&key, &items);
                        Ok(Response::Comments { items })
                    }
                    Err(e) => {
                        // Live fetch failed — fall back to whatever was cached.
                        warn!(%key, "live comment fetch failed, returning cache: {e:#}");
                        Ok(Response::Comments { items: cached })
                    }
                }
            } else {
                // Spawn a background refresh so the *next* read is fresh.
                let s2 = state.clone();
                let k2 = key.clone();
                tokio::spawn(async move { refresh_comments(&s2, &k2).await; });
                Ok(Response::Comments { items: cached })
            }
        }

        Request::DeleteComment { key, comment_id } => {
            let api = JiraApi::from_jira_cli_config()?;
            api.delete_comment(&key, &comment_id).await?;
            let s2 = state.clone();
            let k2 = key.clone();
            tokio::spawn(async move { refresh_ticket_after_mutation(&s2, &k2).await; });
            Ok(Response::Ok)
        }

        Request::Myself => {
            let api = JiraApi::from_jira_cli_config()?;
            let info = api.myself().await?;
            Ok(Response::Myself { info })
        }

        Request::ListMyMentions => {
            // Cache-first. Poll loop refreshes Jira-side roles periodically;
            // GitHub side is refreshed by `refresh_github_mentions` on the same
            // cadence. Async refresh kicks off if any role is older than 5 min.
            const STALE: i64 = 300;
            let (reviewing, mentioned, github, authored) = {
                let cache = state.cache.lock().await;
                (
                    cache.get_mentions("reviewer")?,
                    cache.get_mentions("mentioned")?,
                    cache.get_mentions("github")?,
                    cache.get_mention_keys("authored")?,
                )
            };
            let age_r = state.cache.lock().await.mentions_age_secs("reviewer")?;
            let age_m = state.cache.lock().await.mentions_age_secs("mentioned")?;
            let age_g = state.cache.lock().await.mentions_age_secs("github")?;
            let age_a = state.cache.lock().await.mentions_age_secs("authored")?;
            let stale = age_r.map(|a| a > STALE).unwrap_or(true)
                || age_m.map(|a| a > STALE).unwrap_or(true)
                || age_g.map(|a| a > STALE).unwrap_or(true)
                || age_a.map(|a| a > STALE).unwrap_or(true);
            if reviewing.is_empty() && mentioned.is_empty() && github.is_empty() {
                refresh_my_mentions(&state).await;
                let _ = refresh_github_mentions(&state).await;
                let cache = state.cache.lock().await;
                return Ok(Response::MyMentions {
                    reviewing: cache.get_mentions("reviewer")?,
                    mentioned: cache.get_mentions("mentioned")?,
                    github: cache.get_mentions("github")?,
                    authored: cache.get_mention_keys("authored")?,
                });
            }
            if stale {
                let s2 = state.clone();
                tokio::spawn(async move {
                    refresh_my_mentions(&s2).await;
                    let _ = refresh_github_mentions(&s2).await;
                });
            }
            Ok(Response::MyMentions { reviewing, mentioned, github, authored })
        }

        Request::SetGithubHandle { account_id, handle } => {
            let mut map = jui_core::users_map::UsersMap::load().unwrap_or_default();
            map.set(&account_id, &handle);
            map.save()?;
            Ok(Response::Ok)
        }

        Request::GetGithubHandle { account_id } => {
            let map = jui_core::users_map::UsersMap::load().unwrap_or_default();
            let handle = map.lookup(&account_id).unwrap_or("").to_string();
            Ok(Response::GithubHandle { handle })
        }

        Request::ListPrComments { ticket_key } => {
            let cache = state.cache.lock().await;
            let items = cache.get_pr_comments(&ticket_key)?;
            // Prefer the canonical link table; fall back to the first cached
            // comment's URL so older data (pre-ticket_prs) still works.
            let pr_link = cache
                .get_ticket_pr_url(&ticket_key)?
                .or_else(|| items.first().map(|c| c.pr_url.clone()));
            Ok(Response::PrComments { items, pr_link })
        }

        Request::ResolvePrComment { ticket_key, comment_id } => {
            let (repo, number) = {
                let cache = state.cache.lock().await;
                match cache.get_ticket_pr_meta(&ticket_key)? {
                    Some(m) => m,
                    None => {
                        return Ok(Response::Err {
                            message: format!("no PR on file for {ticket_key}"),
                        });
                    }
                }
            };
            if let Err(e) = jui_core::github::resolve_review_thread(&repo, number, &comment_id).await {
                return Ok(Response::Err { message: format!("{e:#}") });
            }
            // Async refresh — resolved threads still appear in REST, so the
            // pane content doesn't change yet, but a refresh keeps things
            // consistent (replies you might have added land too).
            let s2 = state.clone();
            let k2 = ticket_key.clone();
            tokio::spawn(async move { refresh_pr_comments_for_ticket(&s2, &k2).await; });
            Ok(Response::Ok)
        }

        Request::ReplyToPrComment { ticket_key, parent_kind, parent_id, body } => {
            // Resolve repo + number for the ticket. Prefer the canonical
            // ticket_prs row; fall back to scanning cached pr_comments for a
            // matching ticket so older data still routes.
            let (repo, number) = {
                let cache = state.cache.lock().await;
                if let Some((r, n)) = cache.get_ticket_pr_meta(&ticket_key)? {
                    (r, n)
                } else if let Some(c) = cache.get_pr_comments(&ticket_key)?.into_iter().next() {
                    (c.repo, c.pr_number)
                } else {
                    return Ok(Response::Err {
                        message: format!("no PR on file for {ticket_key}"),
                    });
                }
            };
            let kind = parent_kind.to_ascii_lowercase();
            let result = if kind == "review" && !parent_id.is_empty() {
                jui_core::github::post_review_comment_reply(&repo, number, &parent_id, &body).await
            } else {
                // issue threads and review_wrapper bodies both land as a fresh
                // issue comment — GitHub doesn't thread issue comments.
                jui_core::github::post_pr_comment(&repo, number, &body).await
            };
            if let Err(e) = result {
                return Ok(Response::Err { message: format!("{e:#}") });
            }
            // Async refresh so the next ListPrComments call surfaces the new row.
            let s2 = state.clone();
            let k2 = ticket_key.clone();
            tokio::spawn(async move { refresh_pr_comments_for_ticket(&s2, &k2).await; });
            Ok(Response::Ok)
        }

        Request::SetupDevQaWorktree { ticket_key, repo, pr_number } => {
            setup_devqa_worktree(&state, &ticket_key, &repo, pr_number).await
        }

        Request::SetPrUserState { ticket_key, state: pr_state } => {
            state.cache.lock().await.set_pr_state(&ticket_key, &pr_state)?;
            Ok(Response::Ok)
        }

        Request::GetPrUserStates => {
            let items = state.cache.lock().await.get_all_pr_states()?;
            Ok(Response::PrUserStates { items })
        }

        Request::CreatePullRequest {
            ticket_key,
            title,
            body,
            reviewer_account_id,
            devqa_account_id,
            push_remote,
        } => {
            create_pull_request(
                &state,
                &ticket_key,
                &title,
                &body,
                reviewer_account_id.as_deref(),
                devqa_account_id.as_deref(),
                push_remote.as_deref(),
            )
            .await
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
            let s2 = state.clone();
            let k2 = key.clone();
            tokio::spawn(async move { refresh_ticket_after_mutation(&s2, &k2).await; });
            Ok(Response::Ok)
        }

        Request::ListTransitions { key } => {
            let api = JiraApi::from_jira_cli_config()?;
            let items = api.list_transitions(&key).await?;
            Ok(Response::Transitions { items })
        }

        Request::ListStatuses => {
            let api = JiraApi::from_jira_cli_config()?;
            let items = api.statuses().await?;
            Ok(Response::Statuses { items })
        }

        Request::GetPrDraft { ticket_key } => {
            let draft = state.cache.lock().await.get_pr_draft(&ticket_key)?;
            Ok(Response::PrDraft { draft })
        }
        Request::SavePrDraft { ticket_key, draft } => {
            state.cache.lock().await.upsert_pr_draft(&ticket_key, &draft)?;
            Ok(Response::Ok)
        }
        Request::DeletePrDraft { ticket_key } => {
            state.cache.lock().await.delete_pr_draft(&ticket_key)?;
            Ok(Response::Ok)
        }

        Request::ListWorktreeRemotes { ticket_key } => {
            // Resolve worktree (with project-root fallback, like /review).
            let project_path = {
                let cache = state.cache.lock().await;
                cache.linked_paths(&ticket_key)?.into_iter().next()
            };
            let Some(project_path) = project_path else {
                return Ok(Response::Err {
                    message: format!("no linked project for {ticket_key}"),
                });
            };
            let ticket = match state.jira.view(&ticket_key).await {
                Ok(t) => Some(t),
                Err(_) => state.cache.lock().await.get_ticket(&ticket_key)?,
            };
            let path = ticket
                .as_ref()
                .and_then(|t| jui_core::scm::worktree_path_for_slug(&project_path, &t.branch_slug()))
                .filter(|w| w.exists())
                .unwrap_or(project_path);
            let items = jui_core::github::list_remotes(&path).await?;
            Ok(Response::Remotes { items })
        }

        Request::GetPushRemote { ticket_key } => {
            let project_path = {
                let cache = state.cache.lock().await;
                cache.linked_paths(&ticket_key)?.into_iter().next()
            };
            let Some(p) = project_path else {
                return Ok(Response::PushRemote { name: None });
            };
            let name = state.cache.lock().await.get_push_remote(&p)?;
            Ok(Response::PushRemote { name })
        }

        Request::SetPushRemote { ticket_key, remote_name } => {
            let project_path = {
                let cache = state.cache.lock().await;
                cache.linked_paths(&ticket_key)?.into_iter().next()
            };
            let Some(p) = project_path else {
                return Ok(Response::Err {
                    message: format!("no linked project for {ticket_key}"),
                });
            };
            state.cache.lock().await.set_push_remote(&p, &remote_name)?;
            Ok(Response::Ok)
        }

        Request::CodeReview { ticket_key } => {
            // Resolve worktree the same way the PR-create flow does. Falls
            // back to the linked project root when the per-ticket worktree
            // is missing so `/review` still works on tickets the user hasn't
            // explicitly `s`-started (or whose summary changed after the
            // worktree was created and no longer matches the slug).
            let project_path = {
                let cache = state.cache.lock().await;
                cache.linked_paths(&ticket_key)?.into_iter().next()
            };
            let ticket = match state.jira.view(&ticket_key).await {
                Ok(t) => Some(t),
                Err(_) => state.cache.lock().await.get_ticket(&ticket_key)?,
            };
            let Some(project_path) = project_path else {
                return Ok(Response::Err {
                    message: format!(
                        "no linked project for {ticket_key} — link a repo (L) first"
                    ),
                });
            };
            let worktree = ticket
                .as_ref()
                .and_then(|t| jui_core::scm::worktree_path_for_slug(&project_path, &t.branch_slug()))
                .filter(|w| w.exists())
                .unwrap_or(project_path);
            // Reuse the ticket's Claude session so the review turn lands in
            // the same conversation history fix-sessions resume into.
            let (session_id, resume) = match state.cache.lock().await.get_claude_session(&ticket_key)? {
                Some(id) => (id, true),
                None => {
                    let new_id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
                        .map(|s| s.trim().to_string())
                        .unwrap_or_else(|_| chrono::Utc::now().timestamp_micros().to_string());
                    state.cache.lock().await.set_claude_session(&ticket_key, &new_id)?;
                    (new_id, false)
                }
            };
            let markdown = jui_core::claude::code_review(&session_id, resume, &worktree).await?;
            Ok(Response::ReviewOutput { markdown })
        }

        Request::GetTicketWorktree { ticket_key } => {
            // Read the first linked project + the ticket so we can build the
            // branch slug. Returns the per-ticket worktree when it exists;
            // otherwise falls back to the linked project root so downstream
            // flows (PR-comment chat, fix-sessions) still have a sensible
            // cwd to spawn Claude in.
            let project_path = {
                let cache = state.cache.lock().await;
                cache.linked_paths(&ticket_key)?.into_iter().next()
            };
            let ticket = match state.jira.view(&ticket_key).await {
                Ok(t) => Some(t),
                Err(_) => state.cache.lock().await.get_ticket(&ticket_key)?,
            };
            let path = project_path.map(|p| {
                ticket
                    .as_ref()
                    .and_then(|t| jui_core::scm::worktree_path_for_slug(&p, &t.branch_slug()))
                    .filter(|w| w.exists())
                    .unwrap_or(p)
            });
            Ok(Response::TicketWorktree { path })
        }

        Request::CreateTicket { project, issue_type, summary, body, parent } => {
            let key = state
                .jira
                .create(&project, &issue_type, &summary, body.as_deref(), parent.as_deref())
                .await?;
            // Pull the new ticket into cache + refresh the parent's view so its
            // subtasks list updates without waiting for the next poll tick.
            let s2 = state.clone();
            let k2 = key.clone();
            let parent_key = parent.clone();
            tokio::spawn(async move {
                refresh_ticket_after_mutation(&s2, &k2).await;
                if let Some(p) = parent_key {
                    refresh_ticket_after_mutation(&s2, &p).await;
                }
            });
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
            let s2 = state.clone();
            let k2 = key.clone();
            tokio::spawn(async move { refresh_ticket_after_mutation(&s2, &k2).await; });
            Ok(Response::Ok)
        }

        Request::SetReviewer { key, assignee_id } => {
            let api = JiraApi::from_jira_cli_config()?;
            api.set_reviewer(&key, &assignee_id, &state.config.jira.reviewer_customfield).await?;
            let s2 = state.clone();
            let k2 = key.clone();
            tokio::spawn(async move {
                refresh_ticket_after_mutation(&s2, &k2).await;
                refresh_my_mentions(&s2).await;
            });
            Ok(Response::Ok)
        }

        Request::EditSummary { key, summary } => {
            state.jira.edit_summary(&key, &summary).await?;
            let s2 = state.clone();
            let k2 = key.clone();
            tokio::spawn(async move { refresh_ticket_after_mutation(&s2, &k2).await; });
            Ok(Response::Ok)
        }

        Request::EditDescription { key, body } => {
            state.jira.edit_description(&key, &body).await?;
            let s2 = state.clone();
            let k2 = key.clone();
            tokio::spawn(async move { refresh_ticket_after_mutation(&s2, &k2).await; });
            Ok(Response::Ok)
        }

        Request::ImproveDescription { summary, body } => {
            info!(summary_len = summary.len(), body_len = body.len(), "claude tighten requested");
            let improved = jui_core::claude::improve_description(&summary, &body).await?;
            info!(out_len = improved.len(), "claude tighten done");
            Ok(Response::Improved { body: improved })
        }

        Request::EditPriority { key, priority } => {
            // Priority via REST: jira-cli's `-y` flag is rejected by some Jira
            // instances. The REST PUT endpoint reliably accepts {"name": ...}.
            let api = JiraApi::from_jira_cli_config()?;
            api.set_priority(&key, &priority).await?;
            let s2 = state.clone();
            let k2 = key.clone();
            tokio::spawn(async move { refresh_ticket_after_mutation(&s2, &k2).await; });
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
            let s2 = state.clone();
            let k2 = key.clone();
            tokio::spawn(async move { refresh_ticket_after_mutation(&s2, &k2).await; });
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

/// Re-fetch a single ticket's view + comments from Jira and update the cache.
/// Used after mutations so the next cache read sees fresh data.
async fn refresh_ticket_after_mutation(state: &State, key: &str) {
    if let Ok(t) = state.jira.view(key).await {
        if let Err(e) = state.cache.lock().await.upsert_tickets(std::slice::from_ref(&t)) {
            warn!(%key, "post-mutation ticket upsert: {e:#}");
        }
    }
    refresh_comments(state, key).await;
}

/// Re-fetch the three PR comment surfaces for a ticket and persist the
/// merged set. Called after a reply succeeds so the pane shows the new
/// comment without waiting for the next github-mention warmup tick.
async fn refresh_pr_comments_for_ticket(state: &State, ticket_key: &str) {
    let meta = match state.cache.lock().await.get_ticket_pr_meta(ticket_key) {
        Ok(Some(m)) => m,
        Ok(None) => return,
        Err(e) => { warn!(%ticket_key, "pr_meta lookup: {e:#}"); return; }
    };
    let (repo, number) = meta;
    let mut merged: Vec<jui_core::github::FetchedComment> = Vec::new();
    if let Ok(items) = jui_core::github::pr_comments(&repo, number).await {
        merged.extend(items);
    }
    if let Ok(items) = jui_core::github::pr_review_comments(&repo, number).await {
        merged.extend(items);
    }
    if let Ok(items) = jui_core::github::pr_reviews(&repo, number).await {
        merged.extend(items);
    }
    if let Ok(map) = jui_core::github::review_thread_resolution_map(&repo, number).await {
        for c in merged.iter_mut() {
            if c.kind == "review" {
                if let Some(&resolved) = map.get(&c.id) {
                    c.is_resolved = resolved;
                }
            }
        }
    }
    merged.sort_by(|a, b| a.created.cmp(&b.created));
    // We need the pr_url to satisfy upsert_pr_comments — pull it from the
    // ticket_prs row written when the PR was created.
    let pr_url = state.cache.lock().await.get_ticket_pr_url(ticket_key)
        .ok().flatten().unwrap_or_else(|| {
            format!("https://github.com/{repo}/pull/{number}")
        });
    if let Err(e) = state.cache.lock().await.upsert_pr_comments(
        ticket_key, &pr_url, number, &repo, &merged,
    ) {
        warn!(%ticket_key, "pr comments upsert after reply: {e:#}");
    }
}

async fn refresh_comments(state: &State, key: &str) {
    match state.jira.comments(key).await {
        Ok(items) => {
            if let Err(e) = state.cache.lock().await.upsert_comments(key, &items) {
                warn!(%key, "comment upsert: {e:#}");
            }
        }
        Err(e) => warn!(%key, "comment refresh: {e:#}"),
    }
}

/// Pull every cached ticket's comments and stash them. Pricey on first run
/// (one Jira API call per ticket) but turns Detail-open into a SQLite read
/// after that. Polite delay between calls.
async fn warm_comments(state: &State) {
    let keys: Vec<String> = match state.cache.lock().await.list_tickets(10_000) {
        Ok(t) => t.into_iter().map(|x| x.key).collect(),
        Err(e) => { warn!("comment warmup: list_tickets failed: {e:#}"); return; }
    };
    let total = keys.len();
    let mut fetched = 0usize;
    for k in keys {
        refresh_comments(state, &k).await;
        fetched += 1;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    info!(fetched, total, "comment warmup complete");
}

/// Run the reviewer + @-mention JQLs and store the resulting key lists in the
/// `mentions` table. Uses tickets that should already be in the `tickets` table
/// (poll_loop ensures this); skips upsert of full Ticket rows so we don't
/// thrash the cache from this hot path.
async fn refresh_my_mentions(state: &State) {
    let api = match JiraApi::from_jira_cli_config() {
        Ok(a) => a,
        Err(e) => { warn!("mentions refresh: api config: {e:#}"); return; }
    };
    let me = match api.myself().await {
        Ok(m) => m,
        Err(e) => { warn!("mentions refresh: myself: {e:#}"); return; }
    };
    let display = me.display_name.replace('"', "\\\"");
    let cf_id = state
        .config
        .jira
        .reviewer_customfield
        .strip_prefix("customfield_")
        .unwrap_or(&state.config.jira.reviewer_customfield)
        .to_string();
    let reviewer_jql = format!(
        "cf[{cf_id}] = currentUser() AND assignee != currentUser() AND statusCategory != Done"
    );
    let mention_jql = format!(
        "text ~ \"@{display}\" AND assignee != currentUser() AND statusCategory != Done"
    );
    let reviewing = state.jira.search(&reviewer_jql, 50).await.unwrap_or_default();
    let mut mentioned = state.jira.search(&mention_jql, 50).await.unwrap_or_default();
    let reviewer_keys: std::collections::HashSet<String> =
        reviewing.iter().map(|t| t.key.clone()).collect();
    mentioned.retain(|t| !reviewer_keys.contains(&t.key));

    // Persist the full Ticket rows (so get_mentions can resolve them) AND the
    // lightweight role → key list.
    {
        let mut cache = state.cache.lock().await;
        let _ = cache.upsert_tickets(&reviewing);
        let _ = cache.upsert_tickets(&mentioned);
        let r_keys: Vec<String> = reviewing.iter().map(|t| t.key.clone()).collect();
        let m_keys: Vec<String> = mentioned.iter().map(|t| t.key.clone()).collect();
        let _ = cache.upsert_mentions("reviewer", &r_keys);
        let _ = cache.upsert_mentions("mentioned", &m_keys);
    }
    info!(reviewing = reviewing.len(), mentioned = mentioned.len(), "mentions refreshed");
}

/// End-to-end PR open: discover worktree → push → gh pr create → request
/// reviewer on GitHub → comment on Jira → transition to Code Review.
async fn create_pull_request(
    state: &Arc<State>,
    ticket_key: &str,
    title: &str,
    body: &str,
    reviewer_account_id: Option<&str>,
    devqa_account_id: Option<&str>,
    push_remote: Option<&str>,
) -> Result<Response> {
    use jui_core::github;
    use jui_core::users_map::UsersMap;

    // 1. Look up the worktree path. The ticket's branch_slug() drives the
    //    canonical path. We fall back to scanning git worktrees if that path
    //    doesn't exist (in case the user changed the slug).
    let ticket = match state.jira.view(ticket_key).await {
        Ok(t) => t,
        Err(_) => state.cache.lock().await.get_ticket(ticket_key)?
            .ok_or_else(|| anyhow::anyhow!("ticket {ticket_key} not found"))?,
    };
    let slug = ticket.branch_slug();
    // We need a base "repo root" to compute the worktree path. Pull it from
    // the user's linked projects for this ticket; daemon-side we can't rely
    // on the TUI's cwd.
    let project_path = {
        let cache = state.cache.lock().await;
        cache.linked_paths(ticket_key)?.into_iter().next()
    };
    let Some(project_path) = project_path else {
        return Err(anyhow::anyhow!(
            "no linked project for {ticket_key}; link a repo first (P pane in detail)"
        ));
    };
    // Prefer the per-ticket worktree when it exists; otherwise fall back to
    // the linked project root and push from whatever branch is checked out
    // there. Matches the /review-side fallback so a ticket the user never
    // explicitly `s`-started still produces a PR.
    let worktree = jui_core::scm::worktree_path_for_slug(&project_path, &slug)
        .filter(|w| w.exists())
        .unwrap_or_else(|| project_path.clone());

    // 2. Discover repo + branch via gh.
    let repo = github::repo_slug(&worktree).await?;
    let head_branch = github::current_branch(&worktree).await?;
    info!(%ticket_key, repo, head_branch, "creating PR");

    // 3. Resolve GitHub handles for the picked Jira users.
    let users_map = UsersMap::load().unwrap_or_default();
    let reviewer_gh = match reviewer_account_id {
        Some(id) => users_map.lookup(id).map(|s| s.to_string()),
        None => None,
    };
    let devqa_gh = match devqa_account_id {
        Some(id) => users_map.lookup(id).map(|s| s.to_string()),
        None => None,
    };
    if let Some(rid) = reviewer_account_id {
        if reviewer_gh.is_none() {
            return Err(anyhow::anyhow!(
                "no GitHub handle mapped for reviewer (jira id {rid}); set one and retry"
            ));
        }
    }
    if let Some(qid) = devqa_account_id {
        if devqa_gh.is_none() {
            return Err(anyhow::anyhow!(
                "no GitHub handle mapped for DevQA (jira id {qid}); set one and retry"
            ));
        }
    }

    // 4. Resolve push remote — explicit arg wins, then cached per-project
    //    pref, then fall back to "origin". Persist the explicit choice so
    //    the next PR for this project skips the picker.
    let remote = if let Some(r) = push_remote {
        let _ = state.cache.lock().await.set_push_remote(&project_path, r);
        r.to_string()
    } else if let Ok(Some(r)) = state.cache.lock().await.get_push_remote(&project_path) {
        r
    } else {
        "origin".to_string()
    };
    info!(%ticket_key, %remote, "pushing branch");
    // Push and create PR.
    github::push_branch(&worktree, &head_branch, &remote).await?;
    // Cross-fork PR support: derive the fork owner from the push remote's
    // URL whenever it doesn't match the upstream slug. `gh pr create --head`
    // needs `<owner>:<branch>` form when the branch lives on a different
    // fork, otherwise it looks up the branch on the target repo (upstream)
    // and bails with "No commits between …".
    let head_owner: Option<String> = jui_core::scm::gh_slug_for_remote(&worktree, &remote)
        .and_then(|push_slug| {
            if push_slug == repo {
                None
            } else {
                push_slug.split('/').next().map(|s| s.to_string())
            }
        });
    // Append `DevQA: @<gh-handle>` to the PR body so GitHub notifies the
    // DevQA user — they're not a formal reviewer (so `--add-reviewer`
    // doesn't apply), but the @-mention triggers the bell on their account.
    let pr_body = match &devqa_gh {
        Some(h) => format!("{}\n\nDevQA: @{}", body.trim_end(), h),
        None => body.to_string(),
    };
    let pr = github::create_pr(
        &worktree,
        "develop",
        &head_branch,
        head_owner.as_deref(),
        title,
        &pr_body,
    )
    .await?;
    info!(%ticket_key, url = pr.url, number = pr.number, "PR created");
    if let Ok(repo) = github::repo_slug(&worktree).await {
        let _ = state.cache.lock().await.upsert_ticket_pr(ticket_key, &pr.url, pr.number, &repo);
    }

    // 5. Request reviewer on GitHub side.
    if let Some(handle) = &reviewer_gh {
        if let Err(e) = github::add_reviewer(&worktree, pr.number, handle).await {
            warn!(%ticket_key, ?handle, "add_reviewer failed: {e:#}");
        }
    }

    // 6. Comment on Jira with PR URL + role tags. Use `\n\n` between rows
    // because Jira ADF collapses single newlines into a space — each
    // logical line needs its own paragraph break to render separately.
    let mut comment = format!("PR: {}\n\n", pr.url);
    if let Some(h) = &reviewer_gh { comment.push_str(&format!("Reviewer: @{h}\n\n")); }
    if let Some(h) = &devqa_gh { comment.push_str(&format!("DevQA: @{h}\n\n")); }
    if !body.trim().is_empty() {
        comment.push_str(body);
    }
    if let Err(e) = state.jira.add_comment(ticket_key, &comment).await {
        warn!(%ticket_key, "add_comment after PR failed: {e:#}");
    }

    // 7. Transition to Code Review.
    let api = JiraApi::from_jira_cli_config()?;
    let transitions = api.list_transitions(ticket_key).await.unwrap_or_default();
    let target = transitions.iter().find(|tr| {
        tr.to_status.as_deref().map(|s| s.eq_ignore_ascii_case("code review")).unwrap_or(false)
            || tr.name.to_ascii_lowercase().contains("code review")
    });
    if let Some(tr) = target {
        if let Err(e) = state.jira.transition(ticket_key, &tr.name).await {
            warn!(%ticket_key, "transition to Code Review failed: {e:#}");
        }
    } else {
        warn!(%ticket_key, "no Code Review transition available");
    }

    // 8. Async cache refresh.
    let s2 = state.clone();
    let k2 = ticket_key.to_string();
    tokio::spawn(async move { refresh_ticket_after_mutation(&s2, &k2).await; });

    Ok(Response::PullRequestCreated { url: pr.url, number: pr.number })
}

/// Walk `git remote` for the clone, return the name of any remote whose URL
/// resolves to `target_slug` (`owner/repo`). If none exists, add one named
/// after `desired_name` (typically the PR author's first name) pointing at
/// `https://github.com/<target_slug>.git`, then return that name.
async fn ensure_remote_for_repo(
    clone: &std::path::Path,
    target_slug: &str,
    desired_name: &str,
) -> Result<String> {
    use jui_core::scm::parse_github_slug;
    let target_lc = target_slug.to_ascii_lowercase();

    // Enumerate remotes via `git remote -v` (one line per remote per direction).
    let out = std::process::Command::new("git")
        .args(["-C", clone.to_str().unwrap(), "remote", "-v"])
        .output()
        .context("git remote -v")?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "git remote -v failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut existing: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in stdout.lines() {
        // Format: "<name>\t<url> (fetch|push)"
        let mut parts = line.split_whitespace();
        if let (Some(name), Some(url)) = (parts.next(), parts.next()) {
            existing.entry(name.to_string()).or_insert_with(|| url.to_string());
        }
    }

    // 1. Already have a remote pointing at the right repo? Use it.
    for (name, url) in &existing {
        if let Some(slug) = parse_github_slug(url) {
            if slug.to_ascii_lowercase() == target_lc {
                return Ok(name.clone());
            }
        }
    }

    // 2. Need to add one. Prefer `desired_name`; if it's already taken by a
    //    different repo, suffix with `-pr` to avoid clobber.
    let url = format!("https://github.com/{target_slug}.git");
    let name = if existing.contains_key(desired_name) {
        format!("{desired_name}-pr")
    } else {
        desired_name.to_string()
    };
    if existing.contains_key(&name) {
        let out = std::process::Command::new("git")
            .args(["-C", clone.to_str().unwrap(), "remote", "set-url", &name, &url])
            .output()
            .context("git remote set-url")?;
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "git remote set-url failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        info!(remote = %name, %target_slug, "updated remote URL");
    } else {
        let out = std::process::Command::new("git")
            .args(["-C", clone.to_str().unwrap(), "remote", "add", &name, &url])
            .output()
            .context("git remote add")?;
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "git remote add {name} {url} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        info!(remote = %name, %target_slug, "added remote");
    }
    Ok(name)
}

/// Resolve a local clone for `repo` (matching origin or upstream remote),
/// fetch the PR head as a local branch, and `git worktree add` it.
async fn setup_devqa_worktree(
    state: &Arc<State>,
    ticket_key: &str,
    repo: &str,
    pr_number: u64,
) -> Result<Response> {
    let candidates: Vec<std::path::PathBuf> = state
        .config
        .projects
        .iter()
        .map(|p| p.path.clone())
        .collect();
    let clone = jui_core::scm::find_clone_for_gh_repo(repo, &candidates).ok_or_else(|| {
        anyhow::anyhow!(
            "no local clone matches {repo}. Add the clone to projects \
             ([[projects]] in config.toml or 'p' in the TUI)."
        )
    })?;

    // Local branch name + worktree path.
    let local_branch = format!("devqa-pr-{pr_number}");
    let repo_name = clone
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo");
    let parent = clone.parent().unwrap_or(&clone);
    let worktrees_root = parent.join(format!("{}-worktrees", repo_name));
    std::fs::create_dir_all(&worktrees_root)?;
    let worktree_path = worktrees_root.join(format!("{ticket_key}-devqa"));

    // 1. Find (or add) a remote that points at the PR's upstream repo so we
    //    can fetch `pull/<n>/head`. The user's clone likely has:
    //      origin    → their fork
    //      upstream? → upstream repo (sometimes missing on fork-only clones)
    //    GitHub mirrors every PR's HEAD into the upstream's `pull/N/head` refs
    //    regardless of which fork the PR was opened from, so we just need any
    //    remote that resolves to `repo` (the PR's upstream slug). When we have
    //    to add one, name it after the PR author's first name so the user can
    //    eyeball whose contribution they're reviewing.
    let author_login = jui_core::github::pr_author_login(repo, pr_number).await
        .unwrap_or_default();
    let desired_name = if author_login.is_empty() {
        "contributor".to_string()
    } else {
        jui_core::github::user_first_name_remote_safe(&author_login).await
            .unwrap_or_else(|_| author_login.clone())
    };
    let remote = ensure_remote_for_repo(&clone, repo, &desired_name).await?;
    let refspec = format!("pull/{pr_number}/head:{local_branch}");
    let out = std::process::Command::new("git")
        .args(["-C", clone.to_str().unwrap(), "fetch", "--force", &remote, &refspec])
        .output()
        .context("git fetch pull/<n>/head")?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "git fetch {remote} pull/{pr_number}/head failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    // 2. Create the worktree (or reuse if it already exists).
    if !worktree_path.exists() {
        let out = std::process::Command::new("git")
            .args([
                "-C", clone.to_str().unwrap(),
                "worktree", "add",
                worktree_path.to_str().unwrap(),
                &local_branch,
            ])
            .output()
            .context("git worktree add")?;
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }

    info!(%ticket_key, %repo, pr_number, path = %worktree_path.display(), "DevQA worktree ready");
    Ok(Response::DevQaWorktree {
        path: worktree_path,
        branch: local_branch,
    })
}

/// Pull GitHub PRs the current user has been requested to review (or
/// @-mentioned on) and store them under the `mentions` table with role
/// `"github"`. Each PR's branch name is parsed via `scm::extract_ticket_key`
/// to recover the Jira ticket; PRs without a recognisable key are skipped.
async fn refresh_github_mentions(state: &State) -> Result<()> {
    use jui_core::github;
    let mut prs = github::search_review_requested().await.unwrap_or_default();
    prs.extend(github::notifications().await.unwrap_or_default());
    // Dedupe by URL.
    let mut seen = std::collections::HashSet::new();
    prs.retain(|p| seen.insert(p.url.clone()));

    let mut keys: Vec<String> = Vec::new();
    let mut tickets_to_cache: Vec<jui_core::ticket::Ticket> = Vec::new();
    // (ticket_key, pr) — used after we drop the cache lock to fetch comments.
    let mut pr_for_key: Vec<(String, jui_core::github::PrSummary)> = Vec::new();
    for pr in &prs {
        let Some(key) = jui_core::scm::extract_ticket_key(&pr.head_branch) else {
            continue;
        };
        if keys.contains(&key) { continue; }
        // Pull (or fetch) the ticket and skip closed-state tickets.
        let ticket = match state.cache.lock().await.get_ticket(&key)? {
            Some(t) => Some(t),
            None => state.jira.view(&key).await.ok(),
        };
        let Some(t) = ticket else { continue };
        let status_lc = t.status.to_ascii_lowercase();
        let closed = matches!(
            status_lc.as_str(),
            "done" | "resolved" | "closed" | "archive" | "archived" | "won't do" | "wont do" | "cancelled" | "canceled"
        );
        if closed { continue; }
        // Cache the freshly-viewed ticket if we just fetched it.
        if state.cache.lock().await.get_ticket(&key)?.is_none() {
            tickets_to_cache.push(t);
        }
        keys.push(key.clone());
        pr_for_key.push((key, pr.clone()));
    }
    {
        let mut cache = state.cache.lock().await;
        if !tickets_to_cache.is_empty() {
            let _ = cache.upsert_tickets(&tickets_to_cache);
        }
        let _ = cache.upsert_mentions("github", &keys);
        // Record the canonical PR URL for each ticket — surfaces in the
        // Detail view even when the PR has zero comments.
        for (k, pr) in &pr_for_key {
            let _ = cache.upsert_ticket_pr(k, &pr.url, pr.number, &pr.repo);
        }
    }

    // Authored-by-me open PRs. Map back to ticket keys via branch name; no
    // ticket fetch (the user's own tickets are loaded by the main list).
    let authored = github::search_authored_open().await.unwrap_or_default();
    let mut authored_keys: Vec<String> = Vec::new();
    let mut seen_a: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Pairs of (ticket_key, &PrSummary) — used to upsert the PR url so the
    // Detail view can render it even when there are no PR comments yet.
    let mut authored_pr_for_key: Vec<(String, &jui_core::github::PrSummary)> = Vec::new();
    for pr in &authored {
        if let Some(key) = jui_core::scm::extract_ticket_key(&pr.head_branch) {
            if seen_a.insert(key.clone()) {
                authored_keys.push(key.clone());
                authored_pr_for_key.push((key, pr));
            }
        }
    }
    {
        let mut cache = state.cache.lock().await;
        let _ = cache.upsert_mentions("authored", &authored_keys);
        for (k, pr) in &authored_pr_for_key {
            let _ = cache.upsert_ticket_pr(k, &pr.url, pr.number, &pr.repo);
        }
    }

    // Fetch PR comments for each tied ticket. One round trip per PR — this is
    // the slow part; rate-limit politely. Also probe each PR for an APPROVED
    // review by the current user — if found, auto-mark the user's review state
    // as `completed` (auto-sync from `gh pr review --approve` and friends).
    let my_login = jui_core::github::whoami().await.ok();
    let mut total_comments = 0usize;
    let mut auto_completed = 0usize;
    for (ticket_key, pr) in &pr_for_key {
        // Fetch all three comment surfaces: top-level issue thread, inline
        // review comments (where Copilot leaves its line-by-line feedback),
        // and review summaries. Merge + sort oldest-first so they read as
        // a single conversation in the pane.
        let mut merged: Vec<jui_core::github::FetchedComment> = Vec::new();
        match jui_core::github::pr_comments(&pr.repo, pr.number).await {
            Ok(items) => merged.extend(items),
            Err(e) => warn!(repo = %pr.repo, number = pr.number, "issue comments fetch: {e:#}"),
        }
        match jui_core::github::pr_review_comments(&pr.repo, pr.number).await {
            Ok(items) => merged.extend(items),
            Err(e) => warn!(repo = %pr.repo, number = pr.number, "review comments fetch: {e:#}"),
        }
        match jui_core::github::pr_reviews(&pr.repo, pr.number).await {
            Ok(items) => merged.extend(items),
            Err(e) => warn!(repo = %pr.repo, number = pr.number, "reviews fetch: {e:#}"),
        }
        // GraphQL pass — only the `review` kind comments belong to threads
        // that can be resolved. Failures are non-fatal: comments still cache,
        // just always marked unresolved.
        if let Ok(map) = jui_core::github::review_thread_resolution_map(&pr.repo, pr.number).await {
            for c in merged.iter_mut() {
                if c.kind == "review" {
                    if let Some(&resolved) = map.get(&c.id) {
                        c.is_resolved = resolved;
                    }
                }
            }
        }
        merged.sort_by(|a, b| a.created.cmp(&b.created));
        total_comments += merged.len();
        let _ = state.cache.lock().await.upsert_pr_comments(
            ticket_key,
            &pr.url,
            pr.number,
            &pr.repo,
            &merged,
        );

        if let Some(login) = &my_login {
            if let Ok(Some(state_str)) =
                jui_core::github::my_latest_review_state(&pr.repo, pr.number, login).await
            {
                if state_str.eq_ignore_ascii_case("APPROVED") {
                    let current = state.cache.lock().await.get_pr_state(ticket_key).ok().flatten();
                    if current.as_deref() != Some("completed") {
                        let _ = state.cache.lock().await.set_pr_state(ticket_key, "completed");
                        auto_completed += 1;
                        // Mark the ticket with a "DevQA complete" comment on
                        // both Jira and the PR. Only fires on the transition
                        // into Completed (the current != completed gate
                        // prevents repeats).
                        const DEVQA_COMMENT: &str = "DevQA complete";
                        if let Err(e) = state.jira.add_comment(ticket_key, DEVQA_COMMENT).await {
                            warn!(%ticket_key, "auto DevQA-complete jira comment failed: {e:#}");
                        } else {
                            refresh_comments(state, ticket_key).await;
                        }
                        if let Err(e) = jui_core::github::post_pr_comment(
                            &pr.repo, pr.number, DEVQA_COMMENT,
                        ).await {
                            warn!(repo = %pr.repo, number = pr.number,
                                  "auto DevQA-complete pr comment failed: {e:#}");
                        }
                        info!(%ticket_key, repo = %pr.repo, number = pr.number,
                              "auto-marked PR Completed (gh APPROVED) + posted DevQA comments");
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    info!(
        github_prs = prs.len(),
        tickets = keys.len(),
        pr_comments = total_comments,
        auto_completed,
        "github mentions refreshed"
    );
    Ok(())
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
        // Refresh mentions on every poll tick — cheap (two JQL searches) and
        // keeps the bottom List section / Tree-mode badges in sync with the
        // server even if the user never triggers a mutation. GitHub side
        // surfaces tickets where the user is reviewer / @-mentioned on a PR.
        refresh_my_mentions(&state).await;
        let _ = refresh_github_mentions(&state).await;
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
