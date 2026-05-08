# jui

A keyboard-driven Jira + Confluence client that lives in your terminal. `jui`
ties tickets to your local git checkouts, runs Claude Code with a per-ticket
session, and keeps a hot SQLite cache so navigation feels instant.

> **Setup, install, and configuration** live in [docs/setup.md](docs/setup.md).
> This README focuses on **what jui does** and **how to drive it**.

---

## At a glance

- **Active tickets** in a list with sorting, expand-subtasks, and inline status.
- **Detail pane** with sub-panes for ticket info, linked git projects, sub-tasks,
  and comments — each with focus tab-cycling.
- **Tree view** that walks parent → grandparent → epic so you can create new
  child tickets under the correct ancestor.
- **Kanban board** by status column, with assignee filter and minimisable
  columns.
- **Confluence**: cached space and page listing, fuzzy search, and a built-in
  page viewer that renders attached images inline (kitty graphics or unicode
  half-blocks).
- **Start work** = create a git worktree at `<repo>/../<repo>-worktrees/<slug>`
  and open a tmux pane in it. **Stop work** transitions back to Backlog and
  prompts for a comment.
- **Claude Code launch** with a stable per-ticket session id (resume picks up
  from where you left off, even when work was kicked off in the browser).
- **Assign / set reviewer** modals with a type-to-filter user picker.
- **Archive** (instead of delete — most Jira sites disallow real deletes) with
  a clear confirmation modal.
- **`?`** anywhere shows an overlay listing the current pane's keys.
- **Logging** to `/tmp/jui.log` — both the TUI and daemon write to it.

---

## Screens

> ⚠️ The PNGs below are placeholders. Drop real screenshots into
> `docs/screenshots/` to bring this section to life.

### List view

The default view: tickets that match your "my work" JQL, sortable by updated /
created / priority / status / breadcrumb / project. Tab on a parent ticket
expands its children inline.

![list view](docs/screenshots/list.png)

```
┌ active tickets ────────────────────────────────────────────────┐
│ ● MT-148271  Add Avahi to the rootfs              In Dev       │
│ ● MT-147275  Cross-compile rust toolchain          To Do       │
│ ● MT-147756  Investigate boot crash               In Review    │
│ ▾ MT-138815  Set up firmware CI                   In Progress  │
│   ↳ MT-148274 Wire up coverage tooling             To Do       │
│   ↳ MT-148275 Add nightly artifact upload          To Do       │
└────────────────────────────────────────────────────────────────┘
 j/k move · tab expand subtasks · enter open · n new · s start
 T tree · b board · p projects · f confluence · q quit
```

### Detail pane

Four stacked sub-panes (info / linked projects / subtasks / comments). Tab
cycles focus. Sub-task and comment lists hide / collapse when empty so the
ticket info gets the room.

![detail view](docs/screenshots/detail.png)

```
┌ detail · tab to switch panes ─────────────────────────────────┐
│ MT-138815   Set up firmware CI               ● Story          │
│ assignee: Doug Nappier   reviewer: —    priority: High        │
│ ── description ────────────────────────────────────────────── │
│   Bring up Github Actions on the firmware repo so PRs build  │
│   and run smoke tests against an emulated target.            │
├ linked projects ─────────────────────────────────────────────┤
│ ▶ ~/code/mt-firmware                          (confirmed)    │
├ subtasks (2 of 7 · 5 hidden — A toggle) ─────────────────────┤
│   ☑ MT-148274  Wire up coverage tooling        To Do          │
│   ☑ MT-148275  Add nightly artifact upload     To Do          │
├ comments (3) ────────────────────────────────────────────────┤
│   Doug, 2d ago: I'll grab this once the new runner lands.    │
└──────────────────────────────────────────────────────────────┘
 @ assign  R reviewer  T subtask  C claude  s stop  D archive
```

### Tree view

Press `T` from the list. `jui` walks every ticket of yours up its parent chain
(through Story → Epic, including the Epic Link custom field) and renders the
forest with vim-style expand/collapse. Press `c` on any node to create a child
pre-seeded with the right project + parent + issue type.

![tree view](docs/screenshots/tree.png)

```
┌ tickets — tree (T) ──────────────────────────────────────────┐
│ ▼ ⚡ Epic   MT-100  Embedded Linux for Connect       In Dev   │
│   ▼ ✦ Story  MT-132256  i.MX 8M Mini Cortex-A53     In Dev   │
│     ☑ Task   MT-147275  Cross-compile rust toolchain  To Do  │
│     ☑ Task   MT-148271  Add Avahi to the rootfs       In Dev │
│     ▶ ✦ Story  MT-138815  Set up firmware CI                 │
│ ▼ ⚡ Epic   MT-200  MTConnect dashboard                       │
│     ☑ Task   MT-148291  Add token rotation             To Do │
└──────────────────────────────────────────────────────────────┘
 j/k move · o/Tab toggle · O/C expand all · c create child
 v two-col · Enter detail · q back
```

### Kanban

`b` from the list. Columns map to your project's workflow statuses. `u` opens a
user search to filter by assignee (or save a team).

![kanban view](docs/screenshots/kanban.png)

### Confluence

`f` from the list opens spaces (cached). Drill into a space → page → page
viewer. `/` searches CQL across all pages, `e` opens the rendered markdown in
`$EDITOR`, `S` syncs your edits back via [`mark`](https://github.com/kovetskiy/mark).
Inline images render via kitty graphics protocol when available (works through
tmux passthrough); falls back to halfblock unicode otherwise.

![confluence page viewer](docs/screenshots/confluence.png)

### Modals

`?` overlay (per-pane key reference), Archive confirmation, Assign / Reviewer
picker, and the Help overlay all use centered modals that gray out the screen
behind them.

![help overlay](docs/screenshots/help.png)

---

## Workflow

### Starting work on a ticket

From the list: `s`. From a detail view: `s`. `jui` will:

1. Transition the ticket to **In Dev** (or whatever your workflow calls it).
2. Prompt for time estimate / priority if missing.
3. `git worktree add <repo>/../<repo>-worktrees/<slug>`. If a branch already
   exists whose name contains the ticket key, the worktree attaches to it;
   otherwise a new branch is created from HEAD.
4. `tmux split-window -c <worktree>` — you land in a new pane in the worktree.
5. Launch Claude Code with a per-ticket session id stored in the daemon, so
   `s` again later resumes the same session. (The `C` key inside Detail also
   launches Claude with the same id, which makes "started in browser, picked
   up locally" workflows seamless.)

### Stopping work

`s` again on a ticket that's in an active status (or has a worktree) opens
the **Stop** flow:

1. Transitions the ticket back to **Backlog**.
2. Pops a comment box so you can leave a note. `Esc` skips, `Ctrl-S` submits.

### Archiving

Hard-delete in Jira typically requires admin permissions you don't have.
`D` opens an **Archive** modal that finds the first matching transition out of
`Archive → Won't Do → Cancelled → Closed → Done` and fires it. Status bar +
log line confirm; failures stay on the modal so you can read them.

### Creating tickets

- `n` from the list — blank Create form, no parent.
- `T` on a parent ticket's Detail — Create form pre-seeded as a sub-task of
  the current ticket (with the right child issue type).
- `c` on any node in the Tree view — Create form pre-seeded as a child of
  that node, with project + issue type derived from the parent.

The Create form has fields for project, type, summary, description, estimate,
priority, **assignee**. The assignee row is type-to-filter (≥ 2 chars) with
↑/↓ to navigate the dropdown; blank submit assigns to you.

### Re-assigning / setting a reviewer

From a Detail Info pane:

- `@` opens the **Assign** picker for the current ticket.
- `R` opens the **Reviewer** picker (writes to the configured custom field —
  default `customfield_10015`).

Both modals share the same UX: type to search, ↑/↓ to navigate, Enter to
submit, Esc to cancel. Assignee defaults to "me" on blank Enter; reviewer has
no default (won't fire on empty).

---

## Key reference

Press `?` inside any pane for the live, context-aware list. The bottom row of
the screen always shows the current pane's primary keys.

| Pane | Keys |
| --- | --- |
| **List** | `j`/`k` move · `Tab` expand subtasks · `Enter` open · `r` refresh · `o` sort · `n` new · `s` start · `T` tree · `a` archive · `b` board · `p` projects · `f` confluence · `q` quit |
| **Detail / Info** | `e` edit · `c` comment · `t` transition · `w` time · `i` priority · `P` link project · `T` add subtask · `C` claude · `s` start/stop · `@` assign · `R` reviewer · `D` archive · `Esc` back |
| **Detail / Subtasks** | `Tab` next pane · `j`/`k` · `Enter` open · `a` add · `A` toggle archived · `D` archive · `C` claude |
| **Detail / Comments** | `Tab` next pane · `j`/`k` · `c` reply · `R` regenerate (claude-suggested replies) |
| **Tree** | `j`/`k` · `g`/`G` · `o`/`Tab` toggle · `O`/`C` expand/collapse all · `c` create child · `v` two-column · `Enter` detail · `q` back |
| **Page viewer** | `j`/`k` scroll · `d`/`u` half-page · `/` search · `n`/`N` next/prev match · `e` edit in $EDITOR · `S` sync back · `q` back |
| **Create form** | `Tab`/`Shift-Tab` field · `Enter` next/submit · `F5` / `Ctrl-S` / `Ctrl-Enter` submit anywhere · `Esc` cancel · `↑`/`↓` navigate assignee picker |
| **Modal pickers** | `Esc`/`q` close · `Enter` confirm · `↑`/`↓` (where applicable) |

---

## Architecture (brief)

- **`jui-daemon`** — long-lived background service. Owns the SQLite cache, polls
  Jira on `poll.interval_secs`, runs the hourly *ticket ancestor warmup*
  (so Tree mode is instant), and serves IPC requests over a Unix socket.
- **`jui`** — the TUI front-end + thin CLI subcommands (`start`, `stop`,
  `refresh`, `status`, `tmux-snippet`, `shell-init`). Auto-spawns the daemon
  on first connect.
- **`jui-core`** — shared crate: config, IPC types, jira-cli + REST wrappers,
  SQLite cache, SCM (worktree) helpers, Confluence API.

### Logging

Both binaries append to `/tmp/jui.log` (rolled by date). Tail it while you
work:

```sh
tail -f /tmp/jui.log
```

Override per-target with `RUST_LOG=jui_core=trace ./target/release/jui`.

### State on disk

| Path | What |
| --- | --- |
| `~/.config/jui/config.toml` | global config |
| `~/.local/share/jui/cache.sqlite` | tickets, comments, Confluence pages, ticket↔project links |
| `$XDG_RUNTIME_DIR/jui/jui.sock` | daemon IPC socket |
| `$XDG_RUNTIME_DIR/jui/status` | tmux status snippet output |
| `/tmp/confluence-assets/<page_id>/` | downloaded inline images |
| `/tmp/jui.log` | combined log |

---

## Layout

- `crates/jui-core` — config, paths, jira-cli wrapper, SQLite cache, IPC, SCM
- `crates/jui-daemon` — `jui-daemon` binary
- `crates/jui-tui` — `jui` binary (TUI + CLI)
