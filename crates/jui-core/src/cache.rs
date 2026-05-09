use crate::jira_api::UserInfo;
use crate::ticket::Ticket;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;

pub struct Cache {
    conn: Connection,
}

impl Cache {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tickets (
                key TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                status TEXT NOT NULL,
                assignee TEXT,
                reporter TEXT,
                priority TEXT,
                issue_type TEXT,
                updated TEXT,
                description TEXT,
                labels TEXT NOT NULL DEFAULT '[]',
                fetched_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS tickets_assignee ON tickets(assignee);
            CREATE INDEX IF NOT EXISTS tickets_updated ON tickets(updated);
            CREATE TABLE IF NOT EXISTS notifications (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                ticket_key TEXT NOT NULL,
                message TEXT NOT NULL,
                created_at TEXT NOT NULL,
                seen INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS ticket_projects (
                ticket_key TEXT NOT NULL,
                project_path TEXT NOT NULL,
                linked_at TEXT NOT NULL,
                PRIMARY KEY (ticket_key, project_path)
            );
            CREATE INDEX IF NOT EXISTS ticket_projects_path ON ticket_projects(project_path);
            "#,
        )?;
        // Additive migration for the suggestion workflow: 'confirmed' (user-approved /
        // user-linked), 'suggested' (claude proposed, awaiting user), 'rejected' (user
        // dismissed — kept as a tombstone so we don't re-suggest).
        for col in [
            "state TEXT NOT NULL DEFAULT 'confirmed'",
            "source TEXT NOT NULL DEFAULT 'user'",
        ] {
            let _ = conn.execute(&format!("ALTER TABLE ticket_projects ADD COLUMN {col}"), []);
        }
        conn.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS ticket_projects_state ON ticket_projects(state);
            CREATE TABLE IF NOT EXISTS ticket_implementations (
                ticket_key TEXT PRIMARY KEY,
                markdown TEXT NOT NULL,
                project_paths TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS ticket_claude_sessions (
                ticket_key TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_used_at TEXT NOT NULL
            );
            "#,
        )?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                account_id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                email TEXT,
                fetched_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS users_display_name ON users(display_name);
            CREATE TABLE IF NOT EXISTS teams (
                id   INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                members TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            "#,
        )?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS confluence_spaces (
                key         TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                description TEXT NOT NULL,
                cached_at   INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS confluence_pages (
                id           TEXT PRIMARY KEY,
                title        TEXT NOT NULL,
                has_children INTEGER NOT NULL,
                space_key    TEXT NOT NULL,
                parent_id    TEXT,
                cached_at    INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS confluence_pages_parent
                ON confluence_pages(space_key, parent_id);
            CREATE TABLE IF NOT EXISTS comments (
                ticket_key TEXT NOT NULL,
                idx        INTEGER NOT NULL,
                id         TEXT,
                account_id TEXT,
                author     TEXT NOT NULL,
                created    TEXT NOT NULL,
                body       TEXT NOT NULL,
                cached_at  INTEGER NOT NULL,
                PRIMARY KEY (ticket_key, idx)
            );
            CREATE INDEX IF NOT EXISTS comments_ticket ON comments(ticket_key);
            CREATE TABLE IF NOT EXISTS mentions (
                role       TEXT NOT NULL,        -- 'reviewer' | 'mentioned' | 'github'
                idx        INTEGER NOT NULL,
                ticket_key TEXT NOT NULL,
                cached_at  INTEGER NOT NULL,
                PRIMARY KEY (role, idx)
            );
            CREATE TABLE IF NOT EXISTS pr_comments (
                ticket_key TEXT NOT NULL,
                idx        INTEGER NOT NULL,
                pr_url     TEXT NOT NULL,
                pr_number  INTEGER NOT NULL,
                repo       TEXT NOT NULL,
                author     TEXT NOT NULL,
                created    TEXT NOT NULL,
                body       TEXT NOT NULL,
                cached_at  INTEGER NOT NULL,
                PRIMARY KEY (ticket_key, idx)
            );
            CREATE INDEX IF NOT EXISTS pr_comments_ticket ON pr_comments(ticket_key);
            CREATE TABLE IF NOT EXISTS pr_user_state (
                ticket_key TEXT PRIMARY KEY,
                state      TEXT NOT NULL,        -- 'awaiting' | 'reviewing' | 'completed'
                updated_at INTEGER NOT NULL
            );
            "#,
        )?;
        // Idempotent additive migrations — `ALTER TABLE ADD COLUMN` errors if column
        // already exists, which we ignore.
        for col in [
            "original_estimate_seconds INTEGER",
            "remaining_estimate_seconds INTEGER",
            "time_spent_seconds INTEGER",
            "created TEXT",
            "parent_key TEXT",
            "parent_summary TEXT",
            "parent_issue_type TEXT",
            "grandparent_key TEXT",
            "grandparent_summary TEXT",
        ] {
            let _ = conn.execute(&format!("ALTER TABLE tickets ADD COLUMN {col}"), []);
        }
        Ok(Self { conn })
    }

    pub fn upsert_tickets(&mut self, tickets: &[Ticket]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let now = chrono::Utc::now().to_rfc3339();
        {
            let mut stmt = tx.prepare(
                r#"INSERT INTO tickets
                    (key, summary, status, assignee, reporter, priority, issue_type, updated, description, labels, fetched_at,
                     original_estimate_seconds, remaining_estimate_seconds, time_spent_seconds, created,
                     parent_key, parent_summary, parent_issue_type, grandparent_key, grandparent_summary)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
                   ON CONFLICT(key) DO UPDATE SET
                     summary=excluded.summary,
                     status=excluded.status,
                     assignee=excluded.assignee,
                     reporter=excluded.reporter,
                     priority=excluded.priority,
                     issue_type=excluded.issue_type,
                     updated=excluded.updated,
                     description=excluded.description,
                     labels=excluded.labels,
                     fetched_at=excluded.fetched_at,
                     original_estimate_seconds=COALESCE(excluded.original_estimate_seconds, tickets.original_estimate_seconds),
                     remaining_estimate_seconds=COALESCE(excluded.remaining_estimate_seconds, tickets.remaining_estimate_seconds),
                     time_spent_seconds=COALESCE(excluded.time_spent_seconds, tickets.time_spent_seconds),
                     created=COALESCE(excluded.created, tickets.created),
                     parent_key=excluded.parent_key,
                     parent_summary=COALESCE(excluded.parent_summary, tickets.parent_summary),
                     parent_issue_type=COALESCE(excluded.parent_issue_type, tickets.parent_issue_type),
                     grandparent_key=COALESCE(excluded.grandparent_key, tickets.grandparent_key),
                     grandparent_summary=COALESCE(excluded.grandparent_summary, tickets.grandparent_summary)"#,
            )?;
            for t in tickets {
                let labels = serde_json::to_string(&t.labels)?;
                stmt.execute(params![
                    t.key, t.summary, t.status, t.assignee, t.reporter,
                    t.priority, t.issue_type, t.updated, t.description, labels, now,
                    t.original_estimate_seconds, t.remaining_estimate_seconds, t.time_spent_seconds,
                    t.created,
                    t.parent_key, t.parent_summary, t.parent_issue_type,
                    t.grandparent_key, t.grandparent_summary,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_tickets(&self, limit: i64) -> Result<Vec<Ticket>> {
        let mut stmt = self.conn.prepare(
            r#"SELECT key, summary, status, assignee, reporter, priority, issue_type, updated, description, labels, original_estimate_seconds, remaining_estimate_seconds, time_spent_seconds, created, parent_key, parent_summary, parent_issue_type, grandparent_key, grandparent_summary
               FROM tickets
               ORDER BY updated DESC NULLS LAST
               LIMIT ?1"#,
        )?;
        let rows = stmt.query_map([limit], row_to_ticket)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_ticket(&self, key: &str) -> Result<Option<Ticket>> {
        let mut stmt = self.conn.prepare(
            r#"SELECT key, summary, status, assignee, reporter, priority, issue_type, updated, description, labels, original_estimate_seconds, remaining_estimate_seconds, time_spent_seconds, created, parent_key, parent_summary, parent_issue_type, grandparent_key, grandparent_summary
               FROM tickets WHERE key = ?1"#,
        )?;
        let mut rows = stmt.query_map([key], row_to_ticket)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// For each key in `keys`, return the ticket plus every ancestor reachable via
    /// `parent_key`, transitively. Cache-only: missing tickets are silently skipped
    /// so the caller (TUI) shows a partial tree until the daemon's warmup task fills
    /// the gap. Cycle-safe via a `seen` set.
    pub fn tickets_with_ancestors(&self, keys: &[String]) -> Result<Vec<Ticket>> {
        use std::collections::HashSet;
        let mut out: Vec<Ticket> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: Vec<String> = keys.to_vec();
        while let Some(k) = queue.pop() {
            if !seen.insert(k.clone()) { continue; }
            if let Some(t) = self.get_ticket(&k)? {
                if let Some(pk) = t.parent_key.clone() {
                    if !seen.contains(&pk) { queue.push(pk); }
                }
                out.push(t);
            }
        }
        Ok(out)
    }

    pub fn delete_ticket(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM tickets WHERE key = ?1", [key])?;
        self.conn.execute("DELETE FROM comments WHERE ticket_key = ?1", [key])?;
        self.conn.execute("DELETE FROM pr_comments WHERE ticket_key = ?1", [key])?;
        Ok(())
    }

    pub fn upsert_pr_comments(
        &mut self,
        ticket_key: &str,
        pr_url: &str,
        pr_number: u64,
        repo: &str,
        items: &[(String, String, String)], // (author, created, body)
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM pr_comments WHERE ticket_key = ?1", [ticket_key])?;
        {
            let mut stmt = tx.prepare(
                r#"INSERT INTO pr_comments
                   (ticket_key, idx, pr_url, pr_number, repo, author, created, body, cached_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
            )?;
            for (i, (author, created, body)) in items.iter().enumerate() {
                stmt.execute(rusqlite::params![
                    ticket_key,
                    i as i64,
                    pr_url,
                    pr_number as i64,
                    repo,
                    author,
                    created,
                    body,
                    now,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// User-managed state for a PR (Awaiting / Reviewing / Completed). Does
    /// not come from GitHub — purely a workflow tracker. Returns `None` if no
    /// row, which the TUI treats as `Awaiting` by default.
    pub fn set_pr_state(&mut self, ticket_key: &str, state: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            r#"INSERT INTO pr_user_state (ticket_key, state, updated_at)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(ticket_key) DO UPDATE SET state=excluded.state, updated_at=excluded.updated_at"#,
            rusqlite::params![ticket_key, state, now],
        )?;
        Ok(())
    }

    pub fn get_pr_state(&self, ticket_key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT state FROM pr_user_state WHERE ticket_key = ?1",
        )?;
        let mut rows = stmt.query([ticket_key])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(row.get::<_, String>(0)?));
        }
        Ok(None)
    }

    pub fn get_all_pr_states(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT ticket_key, state FROM pr_user_state")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn get_pr_comments(&self, ticket_key: &str) -> Result<Vec<crate::github::PrComment>> {
        let mut stmt = self.conn.prepare(
            r#"SELECT pr_url, pr_number, repo, author, created, body
               FROM pr_comments WHERE ticket_key = ?1 ORDER BY idx"#,
        )?;
        let rows = stmt.query_map([ticket_key], |r| {
            Ok(crate::github::PrComment {
                ticket_key: ticket_key.to_string(),
                pr_url: r.get::<_, String>(0)?,
                pr_number: r.get::<_, i64>(1)? as u64,
                repo: r.get::<_, String>(2)?,
                author: r.get::<_, String>(3)?,
                created: r.get::<_, String>(4)?,
                body: r.get::<_, String>(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Confirmed local project paths linked to `ticket_key`. Used by the
    /// daemon's PR-create flow to find the worktree.
    pub fn linked_paths(&self, ticket_key: &str) -> Result<Vec<std::path::PathBuf>> {
        let mut stmt = self.conn.prepare(
            "SELECT project_path FROM ticket_projects \
             WHERE ticket_key = ?1 AND state = 'confirmed' ORDER BY linked_at DESC",
        )?;
        let v: Vec<std::path::PathBuf> = stmt
            .query_map([ticket_key], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .map(std::path::PathBuf::from)
            .collect();
        Ok(v)
    }

    /// Replace the cached comments for `ticket_key`. Order is preserved via `idx`.
    pub fn upsert_comments(&mut self, ticket_key: &str, items: &[crate::ticket::Comment]) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM comments WHERE ticket_key = ?1", [ticket_key])?;
        {
            let mut stmt = tx.prepare(
                r#"INSERT INTO comments (ticket_key, idx, id, account_id, author, created, body, cached_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            )?;
            for (i, c) in items.iter().enumerate() {
                stmt.execute(rusqlite::params![
                    ticket_key,
                    i as i64,
                    c.id,
                    c.account_id,
                    c.author,
                    c.created,
                    c.body,
                    now,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_comments(&self, ticket_key: &str) -> Result<Vec<crate::ticket::Comment>> {
        let mut stmt = self.conn.prepare(
            r#"SELECT id, account_id, author, created, body
               FROM comments WHERE ticket_key = ?1 ORDER BY idx"#,
        )?;
        let rows = stmt.query_map([ticket_key], |r| {
            Ok(crate::ticket::Comment {
                id: r.get(0)?,
                account_id: r.get(1)?,
                author: r.get(2)?,
                created: r.get(3)?,
                body: r.get(4)?,
            })
        })?;
        let v: Vec<_> = rows.filter_map(|r| r.ok()).collect();
        Ok(v)
    }

    /// Seconds since these comments were last refreshed. None means cache miss.
    pub fn comments_age_secs(&self, ticket_key: &str) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT MAX(cached_at) FROM comments WHERE ticket_key = ?1",
        )?;
        let now = chrono::Utc::now().timestamp();
        let mut rows = stmt.query([ticket_key])?;
        if let Some(row) = rows.next()? {
            let ts: Option<i64> = row.get(0)?;
            return Ok(ts.map(|t| now - t));
        }
        Ok(None)
    }

    /// Replace the cached `mentions` table for the given role. Order via `idx`.
    pub fn upsert_mentions(&mut self, role: &str, keys: &[String]) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM mentions WHERE role = ?1", [role])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO mentions (role, idx, ticket_key, cached_at) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (i, k) in keys.iter().enumerate() {
                stmt.execute(rusqlite::params![role, i as i64, k, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Resolve cached mention rows back to full Tickets via the `tickets` table,
    /// preserving the role's stored order.
    pub fn get_mentions(&self, role: &str) -> Result<Vec<Ticket>> {
        let mut stmt = self.conn.prepare(
            "SELECT ticket_key FROM mentions WHERE role = ?1 ORDER BY idx",
        )?;
        let keys: Vec<String> = stmt
            .query_map([role], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            if let Some(t) = self.get_ticket(&k)? {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// Age of the freshest mention row for the given role (cache freshness check).
    pub fn mentions_age_secs(&self, role: &str) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare("SELECT MAX(cached_at) FROM mentions WHERE role = ?1")?;
        let now = chrono::Utc::now().timestamp();
        let mut rows = stmt.query([role])?;
        if let Some(row) = rows.next()? {
            let ts: Option<i64> = row.get(0)?;
            return Ok(ts.map(|t| now - t));
        }
        Ok(None)
    }

    pub fn known_keys(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT key FROM tickets")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn link_project(&self, ticket_key: &str, project_path: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO ticket_projects (ticket_key, project_path, linked_at, state, source)
             VALUES (?1, ?2, ?3, 'confirmed', 'user')
             ON CONFLICT(ticket_key, project_path) DO UPDATE SET state='confirmed', source='user'",
            params![ticket_key, project_path, now],
        )?;
        Ok(())
    }

    pub fn unlink_project(&self, ticket_key: &str, project_path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM ticket_projects WHERE ticket_key = ?1 AND project_path = ?2",
            params![ticket_key, project_path],
        )?;
        Ok(())
    }

    /// Insert a 'suggested' row for a ticket→project pair. No-op if any row already
    /// exists for that pair (we don't override prior user decisions).
    pub fn add_suggestion(&self, ticket_key: &str, project_path: &str, source: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR IGNORE INTO ticket_projects (ticket_key, project_path, linked_at, state, source)
             VALUES (?1, ?2, ?3, 'suggested', ?4)",
            params![ticket_key, project_path, now, source],
        )?;
        Ok(())
    }

    /// Promote a 'suggested' row to 'confirmed'.
    pub fn confirm_suggestion(&self, ticket_key: &str, project_path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE ticket_projects SET state='confirmed' WHERE ticket_key = ?1 AND project_path = ?2 AND state='suggested'",
            params![ticket_key, project_path],
        )?;
        Ok(())
    }

    /// Record that the suggester was invoked but found no clear match. Distinct from
    /// 'rejected' so the UI can surface it; user can later 'd'-dismiss to silence.
    pub fn record_no_match(&self, ticket_key: &str, source: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR IGNORE INTO ticket_projects (ticket_key, project_path, linked_at, state, source)
             VALUES (?1, '(none)', ?2, 'no_match', ?3)",
            params![ticket_key, now, source],
        )?;
        Ok(())
    }

    /// Mark a suggestion (or any row) as 'rejected' so we never suggest it again.
    pub fn reject_suggestion(&self, ticket_key: &str, project_path: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO ticket_projects (ticket_key, project_path, linked_at, state, source)
             VALUES (?1, ?2, ?3, 'rejected', 'user')
             ON CONFLICT(ticket_key, project_path) DO UPDATE SET state='rejected'",
            params![ticket_key, project_path, now],
        )?;
        Ok(())
    }

    /// Project paths + state for a ticket. Excludes rejected rows from default callers.
    pub fn projects_for_ticket(&self, ticket_key: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT project_path, state FROM ticket_projects WHERE ticket_key = ?1 AND state != 'rejected' ORDER BY linked_at",
        )?;
        let rows = stmt.query_map([ticket_key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Ticket keys that have NO 'confirmed' row. Used when a new project is added —
    /// re-suggesting on these tickets can help slot the new project into them.
    pub fn tickets_without_confirmed(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT t.key FROM tickets t
             LEFT JOIN ticket_projects tp
                 ON tp.ticket_key = t.key AND tp.state = 'confirmed'
             WHERE tp.ticket_key IS NULL",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Wipe non-binding rows (suggested + no_match + the synthetic '(none)' row even if
    /// rejected) for the given tickets. Real per-project 'rejected' tombstones survive
    /// because the user explicitly dismissed those projects.
    pub fn clear_unconfirmed(&self, ticket_keys: &[String]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "DELETE FROM ticket_projects
             WHERE ticket_key = ?1
               AND (state IN ('suggested', 'no_match') OR project_path = '(none)')",
        )?;
        for k in ticket_keys {
            stmt.execute([k])?;
        }
        Ok(())
    }

    /// Ticket keys that currently have ZERO rows in ticket_projects (not even rejected) —
    /// the daemon should consider these for suggestion.
    pub fn tickets_needing_suggestion(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.key FROM tickets t
             LEFT JOIN ticket_projects tp ON tp.ticket_key = t.key
             WHERE tp.ticket_key IS NULL",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// For each ticket key in `keys`, return its confirmed project paths. Tickets with
    /// no confirmed link are absent from the map.
    pub fn confirmed_projects_for_tickets(
        &self,
        keys: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        if keys.is_empty() {
            return Ok(Default::default());
        }
        let placeholders = std::iter::repeat("?").take(keys.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT ticket_key, project_path FROM ticket_projects
             WHERE state = 'confirmed' AND ticket_key IN ({placeholders})
             ORDER BY linked_at"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = keys.iter().map(|k| k as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out: std::collections::HashMap<String, Vec<String>> = Default::default();
        for row in rows {
            let (k, p) = row?;
            out.entry(k).or_default().push(p);
        }
        Ok(out)
    }

    /// Tickets linked to a project path.
    pub fn tickets_for_project(&self, project_path: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ticket_key FROM ticket_projects WHERE project_path = ?1 ORDER BY linked_at DESC")?;
        let rows = stmt.query_map([project_path], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_implementation(
        &self,
        ticket_key: &str,
        markdown: &str,
        project_paths: &[String],
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let paths_json = serde_json::to_string(project_paths)?;
        self.conn.execute(
            "INSERT INTO ticket_implementations (ticket_key, markdown, project_paths, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(ticket_key) DO UPDATE SET
                markdown = excluded.markdown,
                project_paths = excluded.project_paths,
                updated_at = excluded.updated_at",
            params![ticket_key, markdown, paths_json, now],
        )?;
        Ok(())
    }

    pub fn get_implementation(&self, ticket_key: &str) -> Result<Option<(String, Vec<String>, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT markdown, project_paths, updated_at FROM ticket_implementations WHERE ticket_key = ?1",
        )?;
        let mut rows = stmt.query_map([ticket_key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        match rows.next() {
            Some(row) => {
                let (md, paths_json, updated) = row?;
                let paths: Vec<String> = serde_json::from_str(&paths_json).unwrap_or_default();
                Ok(Some((md, paths, updated)))
            }
            None => Ok(None),
        }
    }

    /// Tickets that have at least one non-rejected ticket_projects row but no
    /// implementation row yet. The caller filters by liveness/availability.
    pub fn tickets_needing_implementation(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT tp.ticket_key FROM ticket_projects tp
             LEFT JOIN ticket_implementations ti ON ti.ticket_key = tp.ticket_key
             WHERE tp.state IN ('confirmed', 'suggested') AND ti.ticket_key IS NULL",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_claude_session(&self, ticket_key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id FROM ticket_claude_sessions WHERE ticket_key = ?1",
        )?;
        let mut rows = stmt.query_map([ticket_key], |r| r.get::<_, String>(0))?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn set_claude_session(&self, ticket_key: &str, session_id: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO ticket_claude_sessions (ticket_key, session_id, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(ticket_key) DO UPDATE SET last_used_at = excluded.last_used_at",
            params![ticket_key, session_id, now],
        )?;
        Ok(())
    }

    pub fn upsert_users(&self, users: &[UserInfo]) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut stmt = self.conn.prepare(
            "INSERT INTO users (account_id, display_name, email, fetched_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id) DO UPDATE SET
               display_name = excluded.display_name,
               email = excluded.email,
               fetched_at = excluded.fetched_at",
        )?;
        for u in users {
            stmt.execute(params![u.account_id, u.display_name, u.email, now])?;
        }
        Ok(())
    }

    /// Return users whose display_name or email contains `query` (case-insensitive).
    /// Empty query returns all users, ordered by display_name.
    /// Falls back to distinct non-null assignees from the tickets table when the users
    /// table is empty (i.e. before the daemon has synced Jira users for the first time).
    /// Returns `(users, from_cache)` where `from_cache` = true means fallback was used.
    pub fn search_users(&self, query: &str) -> Result<(Vec<UserInfo>, bool)> {
        let pattern = format!("%{}%", query.to_ascii_lowercase());
        // Primary: users table.
        let mut stmt = self.conn.prepare(
            "SELECT account_id, display_name, email FROM users
             WHERE lower(display_name) LIKE ?1 OR lower(coalesce(email,'')) LIKE ?1
             ORDER BY display_name
             LIMIT 100",
        )?;
        let rows = stmt.query_map([&pattern], |r| {
            Ok(UserInfo {
                account_id: r.get(0)?,
                display_name: r.get(1)?,
                email: r.get(2)?,
            })
        })?;
        let results: Vec<UserInfo> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if !results.is_empty() {
            return Ok((results, false));
        }
        // Fallback: distinct assignees from cached tickets (available immediately).
        let mut stmt2 = self.conn.prepare(
            "SELECT DISTINCT assignee FROM tickets
             WHERE assignee IS NOT NULL AND lower(assignee) LIKE ?1
             ORDER BY assignee
             LIMIT 100",
        )?;
        let fallback = stmt2.query_map([&pattern], |r| {
            let name: String = r.get(0)?;
            Ok(UserInfo {
                account_id: name.clone(),
                display_name: name,
                email: None,
            })
        })?;
        Ok((fallback.collect::<rusqlite::Result<Vec<_>>>()?, true))
    }

    pub fn save_team(&self, name: &str, members: &[String]) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let json = serde_json::to_string(members)?;
        self.conn.execute(
            "INSERT INTO teams (name, members, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET members = excluded.members",
            params![name, json, now],
        )?;
        Ok(())
    }

    pub fn list_teams(&self) -> Result<Vec<(String, Vec<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, members FROM teams ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (name, json) = row?;
            let members: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
            out.push((name, members));
        }
        Ok(out)
    }

    pub fn delete_team(&self, name: &str) -> Result<()> {
        self.conn.execute("DELETE FROM teams WHERE name = ?1", [name])?;
        Ok(())
    }

    // ── Confluence cache ─────────────────────────────────────────────────────

    pub fn upsert_confluence_spaces(
        &mut self,
        spaces: &[crate::confluence_api::ConfluenceSpace],
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO confluence_spaces (key, name, description, cached_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET
                   name=excluded.name,
                   description=excluded.description,
                   cached_at=excluded.cached_at",
            )?;
            for s in spaces {
                stmt.execute(params![s.key, s.name, s.description, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_confluence_spaces(
        &self,
    ) -> Result<Vec<crate::confluence_api::ConfluenceSpace>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, name, description FROM confluence_spaces ORDER BY name",
        )?;
        let rows: Vec<_> = stmt
            .query_map([], |r| {
                Ok(crate::confluence_api::ConfluenceSpace {
                    key: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Seconds since the oldest confluence_spaces row was cached. None = table empty.
    pub fn confluence_spaces_age_secs(&self) -> Result<Option<u64>> {
        let oldest: Option<i64> = self
            .conn
            .query_row("SELECT MIN(cached_at) FROM confluence_spaces", [], |r| r.get(0))
            .ok()
            .flatten();
        Ok(oldest.map(|t| (chrono::Utc::now().timestamp() - t).max(0) as u64))
    }

    pub fn upsert_confluence_pages(
        &mut self,
        space_key: &str,
        parent_id: Option<&str>,
        pages: &[crate::confluence_api::ConfluencePage],
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let tx = self.conn.transaction()?;
        {
            // Replace the entire list for this (space, parent) slot so deletions propagate.
            match parent_id {
                Some(pid) => tx.execute(
                    "DELETE FROM confluence_pages WHERE parent_id = ?1",
                    params![pid],
                )?,
                None => tx.execute(
                    "DELETE FROM confluence_pages WHERE space_key = ?1 AND parent_id IS NULL",
                    params![space_key],
                )?,
            };
            let mut stmt = tx.prepare(
                "INSERT INTO confluence_pages (id, title, has_children, space_key, parent_id, cached_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                   title=excluded.title,
                   has_children=excluded.has_children,
                   space_key=excluded.space_key,
                   parent_id=excluded.parent_id,
                   cached_at=excluded.cached_at",
            )?;
            for p in pages {
                stmt.execute(params![
                    p.id, p.title, p.has_children as i64,
                    space_key, parent_id, now
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_confluence_pages(
        &self,
        space_key: &str,
        parent_id: Option<&str>,
    ) -> Result<Vec<crate::confluence_api::ConfluencePage>> {
        let rows: Vec<crate::confluence_api::ConfluencePage> = match parent_id {
            Some(pid) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, title, has_children FROM confluence_pages
                     WHERE parent_id = ?1 ORDER BY title",
                )?;
                let v: Vec<_> = stmt.query_map(params![pid], |r| {
                    Ok(crate::confluence_api::ConfluencePage {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        has_children: r.get::<_, i64>(2)? != 0,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
                v
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, title, has_children FROM confluence_pages
                     WHERE space_key = ?1 AND parent_id IS NULL ORDER BY title",
                )?;
                let v: Vec<_> = stmt.query_map(params![space_key], |r| {
                    Ok(crate::confluence_api::ConfluencePage {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        has_children: r.get::<_, i64>(2)? != 0,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
                v
            }
        };
        Ok(rows)
    }

    /// Seconds since the oldest cached row for this (space, parent) slot. None = not cached.
    pub fn confluence_pages_age_secs(
        &self,
        space_key: &str,
        parent_id: Option<&str>,
    ) -> Result<Option<u64>> {
        let oldest: Option<i64> = match parent_id {
            Some(pid) => self
                .conn
                .query_row(
                    "SELECT MIN(cached_at) FROM confluence_pages WHERE parent_id = ?1",
                    params![pid],
                    |r| r.get(0),
                )
                .ok()
                .flatten(),
            None => self
                .conn
                .query_row(
                    "SELECT MIN(cached_at) FROM confluence_pages WHERE space_key = ?1 AND parent_id IS NULL",
                    params![space_key],
                    |r| r.get(0),
                )
                .ok()
                .flatten(),
        };
        Ok(oldest.map(|t| (chrono::Utc::now().timestamp() - t).max(0) as u64))
    }

    pub fn record_notification(&self, kind: &str, key: &str, msg: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO notifications (kind, ticket_key, message, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![kind, key, msg, now],
        )?;
        Ok(())
    }
}

fn row_to_ticket(r: &rusqlite::Row) -> rusqlite::Result<Ticket> {
    let labels_raw: String = r.get(9)?;
    let labels: Vec<String> = serde_json::from_str(&labels_raw).unwrap_or_default();
    Ok(Ticket {
        key: r.get(0)?,
        summary: r.get(1)?,
        status: r.get(2)?,
        assignee: r.get(3)?,
        reporter: r.get(4)?,
        priority: r.get(5)?,
        issue_type: r.get(6)?,
        updated: r.get(7)?,
        description: r.get(8)?,
        labels,
        original_estimate_seconds: r.get(10)?,
        remaining_estimate_seconds: r.get(11)?,
        time_spent_seconds: r.get(12)?,
        created: r.get(13)?,
        parent_key: r.get(14)?,
        parent_summary: r.get(15)?,
        parent_issue_type: r.get(16)?,
        grandparent_key: r.get(17)?,
        grandparent_summary: r.get(18)?,
        linked_projects: vec![],
        subtasks: vec![],
    })
}
