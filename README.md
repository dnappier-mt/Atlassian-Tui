# jui

A Jira TUI + background daemon that wraps [`jira-cli`](https://github.com/ankitpokhrel/jira-cli),
ties tickets to your current git/svn working directory, and (eventually) pushes desktop
notifications when you're mentioned or assigned.

## Prerequisites

- Rust toolchain (`rustc` 1.80+ / `cargo`)
- [`jira-cli`](https://github.com/ankitpokhrel/jira-cli), authenticated (`jira init`)
- `git`, `svn` (whichever you use)
- `libnotify` / `notify-send` for desktop notifications
- `tmux` (optional, for the status bar integration)

## Build

```sh
cargo build --release --workspace
```

Use `--workspace` (or `cargo build --release` from the repo root with no
`--bin` flag) — `cargo run --bin jui` alone will *not* build the daemon, and
the TUI will fail with `spawning jui-daemon: No such file or directory` when
it tries to auto-start it.

Two binaries land in `target/release/`:

- `jui` — TUI + CLI frontend
- `jui-daemon` — background service

Optionally put them on your `PATH`:

```sh
install -m755 target/release/jui target/release/jui-daemon ~/.local/bin/
```

## Run

The TUI auto-spawns the daemon on first connect:

```sh
jui                  # launch TUI
jui status           # daemon status
jui refresh          # force a Jira poll
jui stop             # shut down the daemon
```

### Shell integration (required for SVN, recommended for git)

A child process can't mutate the parent shell's environment, so SVN's
`SVN_JIRA_DESCRIPTION` is set via a shell-side wrapper. Add to `~/.zshrc` (or
`~/.bashrc`):

```sh
eval "$(jui shell-init)"
```

Now `jui start PROJ-123` runs in the calling shell:

- **git repo, no staged files** → `git switch -c proj-123-<slug>`
- **git repo, staged files** → refuses with a hint to commit/stash first
- **svn repo** → exports `SVN_JIRA_DESCRIPTION=proj-123-<slug>`
- **no SCM** → no-op

Inside the TUI, `s` on the selected ticket triggers the same flow (git only —
SVN export needs the shell wrapper, so use `jui start` from a shell for SVN).

### tmux status bar

```sh
echo "$(jui tmux-snippet)" >> ~/.tmux.conf
tmux source-file ~/.tmux.conf
```

The daemon refreshes a status file every 5 s with ticket count and pending
notifications.

## Per-repo config (optional)

Drop a `.jui.toml` at the root of any repo to scope the ticket list:

```toml
project = "ENG"
# or fully custom:
# jql = "project = ENG AND assignee = currentUser() AND status != Done"
```

## Global config

`~/.config/jui/config.toml`:

```toml
[jira]
binary = "jira"                                # path override if needed
my_jql = "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"

[notifications]
on_mention = true
on_assignment = true
tmux_status = false

[poll]
interval_secs = 120
```

## TUI keys

**list** — `j`/`k` move · `enter` open · `r` refresh · `n` new · `s` start work · `q` quit
**detail** — `e` edit summary · `c` comment · `t` transition · `s` start work · `esc` back
**create** — `tab` next field · `enter` submit on summary · `esc` cancel
**comment** — `ctrl+enter` submit · `esc` cancel

## Development

```sh
cargo check --workspace
cargo test --workspace
cargo run --bin jui-daemon       # run daemon in foreground (logs to stderr)
RUST_LOG=info cargo run --bin jui
```

## Layout

- `crates/jui-core` — config, paths, jira-cli wrapper, SQLite cache, IPC, SCM logic
- `crates/jui-daemon` — `jui-daemon` binary
- `crates/jui-tui` — `jui` binary (TUI + CLI subcommands)

State locations (XDG):

- config: `~/.config/jui/config.toml`
- cache:  `~/.local/share/jui/cache.sqlite`
- socket: `$XDG_RUNTIME_DIR/jui/jui.sock`
- status: `$XDG_RUNTIME_DIR/jui/status`
