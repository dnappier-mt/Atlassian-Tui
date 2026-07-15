# AGENTS.md

This file provides guidance to OpenCode and other coding agents when working with code in this repository.

## What this is

`jui` is a keyboard-driven Jira + Confluence client for the terminal. It ties tickets to local git checkouts, launches Claude Code with a per-ticket session, and keeps a hot SQLite cache so navigation feels instant. See `README.md` for the user-facing feature tour and `docs/setup.md` for install/config.

## Build, install, and run

This is a 3-crate Cargo workspace (`crates/jui-core`, `crates/jui-daemon`, `crates/jui-tui`).

```sh
cargo build              # debug build -> target/debug/{jui,jui-daemon}
cargo build --release    # release build -> target/release/{jui,jui-daemon}
cargo test               # run all tests
cargo test -p jui-core rules::      # run one module's tests (substring filter)
cargo test -p jui-core -- --nocapture name_of_test   # single test, show stdout
cargo clippy --all-targets
```

### IMPORTANT: installed binary vs. cargo build

The binary users actually run is the release build installed to `~/.local/bin/jui` and `~/.local/bin/jui-daemon` by `./install.sh`. `cargo build` alone only updates `target/debug/` and leaves the installed binary stale. A change can appear to not work even though it compiled.

After any code change, do all of the following so nothing is left stale:

```sh
cargo build              # debug: fast iteration / type-check
cargo build --release    # release: what gets installed
./install.sh             # copy release binaries to ~/.local/bin
```

`./install.sh` itself runs `cargo build --release --bins` and then copies. Useful flags: `--link` (symlink instead of copy), `--debug` (install the debug profile), `--uninstall`. Always verify the installed binary's timestamp updated with `ls -la ~/.local/bin/jui` when confirming a fix.

## Architecture

Three crates with a strict dependency direction: `jui-tui` and `jui-daemon` both depend on `jui-core`; they do not depend on each other.

- `jui-core`: shared library. Config + XDG paths, the SQLite cache (`cache.rs`, `rusqlite` bundled), IPC types and client (`ipc.rs`), and all external-tool wrappers: `jira_api.rs` / `confluence_api.rs` (Atlassian REST via `curl`), `github.rs` (GitHub via `gh` + `git`), `scm.rs` (git worktrees), `claude.rs` (launching Claude Code), `rules.rs` (the automation rules engine).
- `jui-daemon`: the `jui-daemon` binary. Long-lived background service that owns the SQLite cache, polls Jira on an interval, runs the hourly ticket-ancestor warmup so Tree mode is instant, evaluates the rules engine, and serves requests over a Unix socket.
- `jui-tui`: the `jui` binary: the ratatui TUI plus thin CLI subcommands (`start`, `stop`, `refresh`, `status`, etc.). Auto-spawns the daemon on first connect.

### TUI to daemon IPC

The TUI never touches SQLite or Jira directly. It sends typed requests to the daemon and renders responses. The wire format (`ipc.rs`) is length-prefixed JSON: a 4-byte big-endian length followed by the JSON body, over a Unix socket at `$XDG_RUNTIME_DIR/jui/jui.sock`. `Request` and `Response` are `#[serde(tag = "type")]` enums. When adding a feature that needs new data, add a `Request` variant and matching `Response`, handle it in the daemon dispatch, and call it from the TUI. Keep request handlers cache-first; the daemon's job is to make the TUI feel instant.

### TUI structure

`jui-tui/src/app.rs` holds `App` and the `Mode` enum. Each screen/modal is a `Mode` variant carrying its own form/state struct. `jui-tui/src/ui.rs` is the render layer: `draw()` dispatches on `Mode` to a `draw_<screen>` function. Rendering is pure: it reads `App` and writes a `Frame`. State mutation lives in `app.rs` key handlers. `md.rs` renders markdown for the Confluence page viewer.

### External tools are hard dependencies

Core shells out to external programs rather than embedding clients: `curl` (Jira/Confluence REST), `gh` + `git` (GitHub, worktrees, branches), `claude` (Claude Code sessions), `tmux` (split panes for "start work"), `mark` (Confluence push-back), and `$EDITOR`. Jira auth/config is read from the `jira-cli` config file. The app expects `jira init` to have been run. When touching these areas, the integration point is constructing argv and parsing stdout, not an SDK.

### State on disk

| Path | What |
| --- | --- |
| `~/.config/jui/config.toml` | global config |
| `~/.local/share/jui/cache.sqlite` | tickets, comments, Confluence pages, ticket-project links, github handles |
| `$XDG_RUNTIME_DIR/jui/jui.sock` | daemon IPC socket |
| `/tmp/jui.log` | combined TUI + daemon log, rolled by date |

## Debugging

Both binaries append to `/tmp/jui.log`. Tail it while reproducing an issue:

```sh
tail -f /tmp/jui.log
```

Scope log levels per target, for example `RUST_LOG=jui_core=trace jui`. Because the TUI auto-spawns the daemon, a stale daemon can mask a fix. After reinstalling, kill the running `jui-daemon` so the next `jui` launch spawns the rebuilt one.
