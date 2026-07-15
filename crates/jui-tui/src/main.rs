mod app;
mod md;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use jui_core::ipc::{self, Request, Response, StartWorkReply};
use jui_core::paths;
use jui_core::scm::WorkLocation;
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser)]
#[command(name = "jui", about = "Jira TUI client")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the TUI (default).
    Tui,
    /// Print the shell snippet to eval — needed for SVN env-var propagation.
    ShellInit,
    /// Print a tmux status-right line that reads the daemon's status file.
    TmuxSnippet,
    /// Show daemon status.
    Status,
    /// Refresh the cache from Jira.
    Refresh,
    /// Stop the daemon.
    Stop,
    /// Start work on a ticket from the command line.
    Start { key: String },
    /// Internal: subcommand the shell wrapper invokes to learn what action to take.
    /// Emits a single line: `git <branch>` | `svn <value>` | `none` | `error <msg>` | `staged`.
    #[command(name = "_start-machine")]
    StartMachine { key: String },
    /// Internal: persist an assistant session id from detached launch helpers.
    #[command(name = "_save-assistant-session", hide = true)]
    SaveAssistantSession {
        key: String,
        assistant: String,
        session_id: String,
    },
}

fn main() -> Result<()> {
    // Log to /tmp/jui.log only (stderr would garble the TUI in raw mode).
    let file = tracing_appender::rolling::never("/tmp", "jui.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,jui=debug,jui_core=debug,jui_tui=debug")
            }),
        )
        .with_writer(file_writer)
        .with_ansi(false)
        .init();
    Box::leak(Box::new(guard));
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        match cli.cmd.unwrap_or(Cmd::Tui) {
            Cmd::Tui => run_tui().await,
            Cmd::ShellInit => {
                print!("{}", SHELL_INIT);
                Ok(())
            }
            Cmd::TmuxSnippet => {
                let path = paths::status_file()?;
                println!(
                    "set -g status-right \"#(cat {} 2>/dev/null) #[default]%H:%M\"",
                    path.display()
                );
                Ok(())
            }
            Cmd::Status => cmd_status().await,
            Cmd::Refresh => cmd_refresh().await,
            Cmd::Stop => cmd_stop().await,
            Cmd::Start { key } => cmd_start(key).await,
            Cmd::StartMachine { key } => cmd_start_machine(key).await,
            Cmd::SaveAssistantSession {
                key,
                assistant,
                session_id,
            } => cmd_save_assistant_session(key, assistant, session_id).await,
        }
    })
}

const SHELL_INIT: &str = r#"# jui shell integration — eval "$(jui shell-init)"
jui() {
    case "$1" in
        start)
            shift
            local out
            out=$(command jui _start-machine "$1") || { echo "$out" >&2; return 1; }
            local kind value
            kind=${out%% *}
            value=${out#* }
            case "$kind" in
                git)   echo "switched to branch: $value" ;;
                svn)   export SVN_JIRA_DESCRIPTION="$value"; echo "exported SVN_JIRA_DESCRIPTION=$value" ;;
                none)  echo "no SCM detected; nothing to do" ;;
                staged) echo "git has staged changes; commit or stash before starting work" >&2; return 1 ;;
                error) echo "${value}" >&2; return 1 ;;
                *)     echo "unexpected: $out" >&2; return 1 ;;
            esac
            ;;
        *)
            command jui "$@"
            ;;
    esac
}
"#;

async fn ensure_daemon() -> Result<tokio::net::UnixStream> {
    if let Ok(s) = ipc::connect().await {
        return Ok(s);
    }
    spawn_daemon()?;
    // poll briefly
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(s) = ipc::connect().await {
            return Ok(s);
        }
    }
    Err(anyhow::anyhow!("could not connect to daemon"))
}

fn spawn_daemon() -> Result<()> {
    // Resolve sibling binary `jui-daemon`.
    let exe = std::env::current_exe().context("current_exe")?;
    let dir = exe.parent().unwrap_or(std::path::Path::new("."));
    let candidate = dir.join("jui-daemon");
    let bin: PathBuf = if candidate.exists() {
        candidate
    } else {
        PathBuf::from("jui-daemon")
    };
    Command::new(bin)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning jui-daemon")?;
    Ok(())
}

async fn cmd_status() -> Result<()> {
    let mut s = ensure_daemon().await?;
    match ipc::send_request(&mut s, &Request::Status).await? {
        Response::Status { status: st } => {
            println!("pid:        {}", st.pid);
            println!("started:    {}", st.started_at);
            println!(
                "last poll:  {}",
                st.last_poll_at.unwrap_or_else(|| "<never>".into())
            );
            println!("cached:     {} tickets", st.cached_tickets);
        }
        Response::Err { message } => println!("error: {message}"),
        other => println!("unexpected: {other:?}"),
    }
    Ok(())
}

async fn cmd_refresh() -> Result<()> {
    let mut s = ensure_daemon().await?;
    match ipc::send_request(&mut s, &Request::Refresh { jql: None }).await? {
        Response::Ok => {
            println!("ok");
            Ok(())
        }
        Response::Err { message } => Err(anyhow::anyhow!(message)),
        other => Err(anyhow::anyhow!("unexpected: {other:?}")),
    }
}

async fn cmd_stop() -> Result<()> {
    let mut s = ipc::connect().await.context("daemon not running")?;
    let _ = ipc::send_request(&mut s, &Request::Shutdown).await?;
    println!("daemon stopped");
    Ok(())
}

async fn cmd_start(key: String) -> Result<()> {
    println!(
        "note: for full SVN support, eval the shell wrapper: eval \"$(jui shell-init)\" then run `jui start {key}`"
    );
    let cwd = std::env::current_dir()?;
    let mut s = ensure_daemon().await?;
    match ipc::send_request(
        &mut s,
        &Request::StartWork {
            key,
            cwd,
            slug: None,
            location: WorkLocation::Worktree,
        },
    )
    .await?
    {
        Response::StartWork {
            reply:
                StartWorkReply::GitWorktree {
                    branch,
                    path,
                    created_branch,
                    attached_existing_worktree,
                },
        } => {
            let action = if attached_existing_worktree {
                "reusing existing worktree"
            } else if created_branch {
                "created new branch + worktree"
            } else {
                "attached worktree to existing branch"
            };
            println!("{action}\n  branch: {branch}\n  path:   {}", path.display());
            Ok(())
        }
        Response::StartWork {
            reply:
                StartWorkReply::GitBranchInRepo {
                    branch,
                    path,
                    created_branch,
                    already_on_branch,
                },
        } => {
            let action = if already_on_branch {
                "already on branch"
            } else if created_branch {
                "created new branch in repo"
            } else {
                "checked out existing branch in repo"
            };
            println!("{action}\n  branch: {branch}\n  repo:   {}", path.display());
            Ok(())
        }
        Response::StartWork {
            reply: StartWorkReply::SvnExport { value },
        } => {
            println!(
                "would export SVN_JIRA_DESCRIPTION={value} (use shell wrapper to actually export)"
            );
            Ok(())
        }
        Response::StartWork {
            reply: StartWorkReply::NoScm,
        } => {
            println!("no SCM detected");
            Ok(())
        }
        Response::Err { message } => Err(anyhow::anyhow!(message)),
        other => Err(anyhow::anyhow!("unexpected: {other:?}")),
    }
}

async fn cmd_start_machine(key: String) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let mut s = ensure_daemon().await?;
    let resp = ipc::send_request(
        &mut s,
        &Request::StartWork {
            key,
            cwd,
            slug: None,
            location: WorkLocation::Worktree,
        },
    )
    .await?;
    match resp {
        Response::StartWork {
            reply: StartWorkReply::GitWorktree { path, .. },
        } => {
            println!("worktree {}", path.display());
        }
        Response::StartWork {
            reply: StartWorkReply::GitBranchInRepo { path, .. },
        } => {
            println!("repo {}", path.display());
        }
        Response::StartWork {
            reply: StartWorkReply::SvnExport { value },
        } => println!("svn {value}"),
        Response::StartWork {
            reply: StartWorkReply::NoScm,
        } => println!("none"),
        Response::Err { message } => println!("error {message}"),
        other => println!("error unexpected:{other:?}"),
    }
    Ok(())
}

async fn cmd_save_assistant_session(
    key: String,
    assistant: String,
    session_id: String,
) -> Result<()> {
    let mut s = ensure_daemon().await?;
    match ipc::send_request(
        &mut s,
        &Request::SaveAssistantSession {
            ticket_key: key,
            assistant,
            session_id,
        },
    )
    .await?
    {
        Response::Ok => Ok(()),
        Response::Err { message } => Err(anyhow::anyhow!(message)),
        other => Err(anyhow::anyhow!("unexpected: {other:?}")),
    }
}

async fn run_tui() -> Result<()> {
    ensure_daemon().await?;
    app::run().await
}
