# Setup

End-to-end install + first-run guide for `jui`. After this, head to the
[README](../README.md) for what to do with it.

---

## 1. Prerequisites

| What | Why | Where to get it |
| --- | --- | --- |
| **Rust** 1.80+ | build `jui` and `jui-daemon` | <https://rustup.rs> |
| **`jira-cli`** | underlying Jira CLI; `jui` shells out to it for searches and a few mutations | <https://github.com/ankitpokhrel/jira-cli/releases> or `brew install jira-cli` / `snap install jira` |
| **`git`** | `start work` creates worktrees | distro package |
| **`curl`** | REST calls (assignee, reviewer, archive, Confluence images) | distro package |
| **`tmux`** ≥ 3.4 | required for `start work` (opens a pane in the worktree), inline images via kitty graphics, and the status bar snippet | distro package |
| **`mark`** *(optional)* | `S` in the page viewer syncs your edited markdown back to Confluence | <https://github.com/kovetskiy/mark> (`go install github.com/kovetskiy/mark@latest`) |
| **`pandoc`** *(optional, recommended)* | best-quality HTML → markdown when opening a Confluence page; falls back to a built-in converter when missing | distro package |
| **`libnotify` / `notify-send`** *(optional)* | desktop notifications for assignments / mentions | distro package |

You'll also want a terminal that supports the **kitty graphics protocol**
if you want inline Confluence images at full fidelity (Ghostty, Kitty,
WezTerm, recent VTE). Anything else falls back to unicode half-blocks.

---

## 2. Get an Atlassian API token

1. Go to <https://id.atlassian.com/manage-profile/security/api-tokens>.
2. Click **Create API token** (or "Create API token with scopes" if your
   org enforces scoped tokens — pick `read:jira-work`, `write:jira-work`,
   `read:confluence-content.all`, `write:confluence-content`).
3. Copy the token *now* — you can't see it again.
4. Export it from your shell rc (`~/.zshrc` / `~/.bashrc`):

   ```sh
   export JIRA_API_TOKEN='paste-the-token-here'
   ```

   `jui` also reads `CONFLUENCE_API_TOKEN`; if it's unset, Confluence calls
   reuse `JIRA_API_TOKEN` (works on Atlassian Cloud where one token covers
   both products). Set both only if you have separate tokens.

5. Reload your shell so the env var is set: `exec $SHELL -l`.

---

## 3. Configure `jira-cli`

`jui` reads the same config file that `jira-cli` writes (`~/.config/.jira/.config.yml`),
so initialising `jira-cli` once also configures `jui`.

```sh
jira init
```

It will prompt for:

- **Server URL** — `https://<your-org>.atlassian.net`
- **Login** — your Atlassian account email
- **Auth type** — `api_token` (uses `JIRA_API_TOKEN` from your environment)
- **Default project / board** — set to your most-used Jira project key
  (e.g. `MT`, `ENG`); both can be changed per-call later.

Verify it works:

```sh
jira issue list -q "assignee = currentUser()"
```

If that returns rows, `jui` will too.

> **Confluence note**: `jira init` only writes Jira fields. Confluence
> reuses the same server and login. If your Confluence site is on a
> different host (rare on Cloud), set `CONFLUENCE_HOST` env var or edit
> `~/.config/.jira/.config.yml` to add `confluence_server:`.

---

## 4. Build and install `jui`

Clone and build:

```sh
git clone https://github.com/dnappier-mt/Atlassian-Tui.git jui
cd jui
cargo build --release --workspace
```

> Use `--workspace` (or just `cargo build --release` from the repo root
> with no `--bin` flag). `cargo run --bin jui` alone won't build the
> daemon, and the TUI fails with `spawning jui-daemon: No such file or
> directory` when it tries to auto-start it.

Two binaries land in `target/release/`:

- `jui` — TUI + thin CLI subcommands
- `jui-daemon` — background service (auto-spawned by `jui` on first
  connect, but it's nice to have on PATH so you can stop / restart it
  manually)

Put both on your `PATH`:

```sh
install -m755 target/release/jui target/release/jui-daemon ~/.local/bin/
```

Make sure `~/.local/bin` is in your `$PATH`:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc   # or ~/.bashrc
```

---

## 5. Shell integration (recommended)

`jui start <KEY>` from a shell needs to be able to mutate the parent
shell's environment (for SVN repos that export `SVN_JIRA_DESCRIPTION`)
and to print readable output. Add this to your shell rc:

```sh
eval "$(jui shell-init)"
```

Now `jui start PROJ-123` from a shell:

- in a **git** repo → creates `<repo>/../<repo>-worktrees/<slug>` with the
  right branch, opens a `tmux split-window` in it, and launches Claude Code
  with a per-ticket session id
- in an **svn** repo → exports `SVN_JIRA_DESCRIPTION=proj-123-<slug>`
- in a non-SCM dir → no-op

Inside the TUI, `s` does the same flow.

---

## 6. tmux configuration

Two pieces matter:

### 6a. Status bar (optional)

Show ticket counts + pending notifications in your tmux status:

```sh
echo "$(jui tmux-snippet)" >> ~/.tmux.conf
tmux source-file ~/.tmux.conf
```

### 6b. True color + kitty graphics passthrough (recommended for inline images)

```tmux
# in ~/.tmux.conf or ~/.tmux.conf.local
set -as terminal-features ",*:RGB"
set -g  allow-passthrough on
```

After reload, **open a new pane** for the changes to apply (per-pane
setting). Inside the new pane, `tail -f /tmp/jui.log` to verify nothing
explodes when you open a Confluence page.

---

## 7. `jui` config (optional)

Drop a `~/.config/jui/config.toml` for global settings:

```toml
[jira]
binary               = "jira"   # path override if needed
my_jql               = "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
# Reviewer custom field id. Default is customfield_10015 ("Code Reviewer"
# on Cloud). Find your site's id with:
#   curl -u "<email>:<token>" "<server>/rest/api/3/field" \
#     | jq '.[] | select(.name|test("review";"i")) | {id,name}'
reviewer_customfield = "customfield_10015"

[notifications]
on_mention    = true
on_assignment = true
tmux_status   = false

[poll]
interval_secs = 120
```

Per-repo overrides go in `.jui.toml` at any repo root:

```toml
project = "ENG"
# or fully custom:
# jql = "project = ENG AND assignee = currentUser() AND status != Done"
```

---

## 8. First run

```sh
jui            # auto-starts the daemon, opens the TUI
```

In another terminal, watch the log:

```sh
tail -f /tmp/jui.log
```

The daemon will spend the first 30–90 seconds doing an initial cache
warmup (Confluence pages recursively, ticket parents, full ticket views).
After that, `T`-tree, drilling pages, opening tickets all read from
SQLite and feel instant.

Useful CLI subcommands:

```sh
jui status     # is the daemon running? when did it last poll?
jui refresh    # force a Jira poll now
jui stop       # shut the daemon down (e.g. before re-installing)
```

---

## 9. Troubleshooting

| Symptom | Likely cause & fix |
| --- | --- |
| `spawning jui-daemon: No such file or directory` | You ran `cargo run --bin jui` instead of `cargo build --release --workspace`. Build the daemon too, or install it on `$PATH`. |
| `JIRA_API_TOKEN not set; needed to ...` | Token env var isn't exported in the shell that launched `jui-daemon`. Restart the daemon (`jui stop` then `jui`) from a shell that has the env var. |
| `403 You do not have permission to delete issues in this project` | Your account lacks Delete permission on that project. Use `D archive` instead — the modal transitions the ticket via Archive / Won't Do / Cancelled / Closed / Done. |
| Tree mode shows tickets but no parents | First run, ancestor warmup hasn't finished. Wait ~30s and reopen. Or check `tail -f /tmp/jui.log` for the "ticket warmup complete" line. |
| Reviewer assignment "all reviewer PUT shapes rejected" | Your site uses a different custom field id. Find it via the `curl` snippet in section 7 and set `reviewer_customfield` in `~/.config/jui/config.toml`, then `jui stop`. |
| Inline images render as garbled placeholder characters (`􎻮`) | tmux passthrough isn't enabled or the outer terminal lacks kitty graphics. See section 6b; or set `JUI_IMAGE_PROTOCOL=halfblocks` to fall back. |
| The detail "loading…" never resolves | Daemon down or wedged. `jui stop` then re-launch and tail the log. |

---

## 10. Where state lives

| Path | What |
| --- | --- |
| `~/.config/.jira/.config.yml` | jira-cli config (server, login, default project) |
| `~/.config/jui/config.toml` | jui global config |
| `<repo>/.jui.toml` | per-repo overrides |
| `~/.local/share/jui/cache.sqlite` | tickets, comments, Confluence pages |
| `$XDG_RUNTIME_DIR/jui/jui.sock` | daemon IPC socket |
| `$XDG_RUNTIME_DIR/jui/status` | tmux status snippet output |
| `/tmp/confluence-assets/<page_id>/` | downloaded inline images |
| `/tmp/jui.log` | combined log (daemon + TUI) |
