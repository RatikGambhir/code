//! Cockpit's application database.
//!
//! [`Database`] is a thin, typed layer over one SQLite connection in WAL mode.
//! [`DbHandle`] moves that connection onto a dedicated thread so the GPUI
//! render loop never waits on disk: callers submit closures and await the
//! result.

mod schema;
mod service;

use std::path::Path;

use anyhow::{Context as _, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub use service::DbHandle;

/// Seconds since the Unix epoch.
pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::configure(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::configure(Connection::open_in_memory()?)
    }

    fn configure(conn: Connection) -> Result<Self> {
        // WAL lets the UI read while the event writer appends; NORMAL sync is
        // durable across app crashes, which is what matters for this data.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: usize = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?
            as usize;
        if version > schema::MIGRATIONS.len() {
            anyhow::bail!(
                "database schema version {version} is newer than this build supports ({})",
                schema::MIGRATIONS.len()
            );
        }
        for (index, migration) in schema::MIGRATIONS.iter().enumerate().skip(version) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(migration)
                .with_context(|| format!("applying migration {}", index + 1))?;
            tx.pragma_update(None, "user_version", (index + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    pub fn journal_mode(&self) -> Result<String> {
        Ok(self
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))?)
    }
}

// ---------------------------------------------------------------------------
// Workspaces and tabs
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceRecord {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub last_opened: i64,
    pub layout_json: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabKind {
    File,
    Browser,
    Diff,
}

impl TabKind {
    fn as_str(self) -> &'static str {
        match self {
            TabKind::File => "file",
            TabKind::Browser => "browser",
            TabKind::Diff => "diff",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "file" => Some(TabKind::File),
            "browser" => Some(TabKind::Browser),
            "diff" => Some(TabKind::Diff),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TabRecord {
    pub pane: i64,
    pub kind: TabKind,
    /// A file path, URL, or diff target depending on `kind`.
    pub resource: String,
    pub active: bool,
}

impl Database {
    /// Records that `path` was opened now, creating the workspace on first use.
    pub fn touch_workspace(&mut self, path: &str, name: &str) -> Result<WorkspaceRecord> {
        let now = now_ts();
        self.conn.execute(
            "INSERT INTO workspaces (path, name, last_opened) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET last_opened = excluded.last_opened, name = excluded.name",
            params![path, name, now],
        )?;
        self.workspace_by_path(path)?
            .context("workspace vanished after upsert")
    }

    pub fn workspace_by_path(&self, path: &str) -> Result<Option<WorkspaceRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path, name, last_opened, layout_json FROM workspaces WHERE path = ?1",
                params![path],
                workspace_from_row,
            )
            .optional()?)
    }

    pub fn recent_workspaces(&self, limit: usize) -> Result<Vec<WorkspaceRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, name, last_opened, layout_json FROM workspaces
             ORDER BY last_opened DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], workspace_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_layout(&mut self, workspace_id: i64, layout_json: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE workspaces SET layout_json = ?2 WHERE id = ?1",
            params![workspace_id, layout_json],
        )?;
        Ok(())
    }

    /// Replaces the workspace's open tabs in one transaction.
    pub fn save_tabs(&mut self, workspace_id: i64, tabs: &[TabRecord]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM tabs WHERE workspace_id = ?1", params![workspace_id])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO tabs (workspace_id, pane, kind, resource, position, active)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (position, tab) in tabs.iter().enumerate() {
                insert.execute(params![
                    workspace_id,
                    tab.pane,
                    tab.kind.as_str(),
                    tab.resource,
                    position as i64,
                    tab.active
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_tabs(&self, workspace_id: i64) -> Result<Vec<TabRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT pane, kind, resource, active FROM tabs
             WHERE workspace_id = ?1 ORDER BY pane, position",
        )?;
        let rows = stmt.query_map(params![workspace_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?;
        let mut tabs = Vec::new();
        for row in rows {
            let (pane, kind, resource, active) = row?;
            // An unknown kind comes from a newer build; skip rather than fail.
            if let Some(kind) = TabKind::parse(&kind) {
                tabs.push(TabRecord {
                    pane,
                    kind,
                    resource,
                    active,
                });
            }
        }
        Ok(tabs)
    }
}

fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        id: row.get(0)?,
        path: row.get(1)?,
        name: row.get(2)?,
        last_opened: row.get(3)?,
        layout_json: row.get(4)?,
    })
}

// ---------------------------------------------------------------------------
// Browser profiles and history
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserProfileRecord {
    pub id: i64,
    pub name: String,
    pub storage_dir: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    pub url: String,
    pub title: Option<String>,
    pub visited_at: i64,
}

impl Database {
    pub fn ensure_browser_profile(
        &mut self,
        name: &str,
        storage_dir: &str,
    ) -> Result<BrowserProfileRecord> {
        self.conn.execute(
            "INSERT INTO browser_profiles (name, storage_dir, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO NOTHING",
            params![name, storage_dir, now_ts()],
        )?;
        Ok(self.conn.query_row(
            "SELECT id, name, storage_dir FROM browser_profiles WHERE name = ?1",
            params![name],
            |row| {
                Ok(BrowserProfileRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    storage_dir: row.get(2)?,
                })
            },
        )?)
    }

    pub fn record_visit(&mut self, profile_id: Option<i64>, url: &str, title: Option<&str>) -> Result<()> {
        // Collapse reloads and redirects that land on the same URL within a
        // few seconds into one entry.
        let now = now_ts();
        let updated = self.conn.execute(
            "UPDATE browser_history SET title = COALESCE(?3, title), visited_at = ?4
             WHERE id = (SELECT id FROM browser_history WHERE url = ?2
                         AND profile_id IS ?1 AND visited_at >= ?4 - 10
                         ORDER BY visited_at DESC LIMIT 1)",
            params![profile_id, url, title, now],
        )?;
        if updated == 0 {
            self.conn.execute(
                "INSERT INTO browser_history (profile_id, url, title, visited_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![profile_id, url, title, now],
            )?;
        }
        Ok(())
    }

    /// Most recent distinct URLs matching `query` in their URL or title.
    pub fn search_history(&self, query: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = self.conn.prepare(
            "SELECT url, title, MAX(visited_at) AS last FROM browser_history
             WHERE url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'
             GROUP BY url ORDER BY last DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            Ok(HistoryEntry {
                url: row.get(0)?,
                title: row.get(1)?,
                visited_at: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn clear_history(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM browser_history", [])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Agent threads and events
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct AgentThreadRecord {
    pub thread_id: String,
    pub workspace_id: Option<i64>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentEventRecord {
    pub id: i64,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at: i64,
}

impl Database {
    pub fn upsert_agent_thread(&mut self, thread: &AgentThreadRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agent_threads (thread_id, workspace_id, title, model, cwd, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(thread_id) DO UPDATE SET
                title = COALESCE(excluded.title, agent_threads.title),
                model = COALESCE(excluded.model, agent_threads.model),
                cwd = COALESCE(excluded.cwd, agent_threads.cwd),
                updated_at = excluded.updated_at",
            params![
                thread.thread_id,
                thread.workspace_id,
                thread.title,
                thread.model,
                thread.cwd,
                thread.created_at,
                thread.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn touch_agent_thread(&mut self, thread_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE agent_threads SET updated_at = ?2 WHERE thread_id = ?1",
            params![thread_id, now_ts()],
        )?;
        Ok(())
    }

    pub fn agent_threads(&self, workspace_id: i64, include_archived: bool) -> Result<Vec<AgentThreadRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT thread_id, workspace_id, title, model, cwd, created_at, updated_at
             FROM agent_threads WHERE workspace_id = ?1 AND (?2 OR archived = 0)
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![workspace_id, include_archived], |row| {
            Ok(AgentThreadRecord {
                thread_id: row.get(0)?,
                workspace_id: row.get(1)?,
                title: row.get(2)?,
                model: row.get(3)?,
                cwd: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn archive_agent_thread(&mut self, thread_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE agent_threads SET archived = 1 WHERE thread_id = ?1",
            params![thread_id],
        )?;
        Ok(())
    }

    pub fn append_agent_event(
        &mut self,
        thread_id: &str,
        turn_id: Option<&str>,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO agent_events (thread_id, turn_id, event_type, payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![thread_id, turn_id, event_type, payload.to_string(), now_ts()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn agent_events(&self, thread_id: &str, after_id: i64, limit: usize) -> Result<Vec<AgentEventRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, thread_id, turn_id, event_type, payload, created_at FROM agent_events
             WHERE thread_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![thread_id, after_id, limit as i64], |row| {
            let payload: String = row.get(4)?;
            Ok(AgentEventRecord {
                id: row.get(0)?,
                thread_id: row.get(1)?,
                turn_id: row.get(2)?,
                event_type: row.get(3)?,
                payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Drops events older than `before_ts`, keeping the log bounded.
    pub fn prune_agent_events(&mut self, before_ts: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM agent_events WHERE created_at < ?1",
            params![before_ts],
        )?)
    }
}

// ---------------------------------------------------------------------------
// Token usage
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}

impl TokenCounts {
    pub fn add(&mut self, other: &TokenCounts) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_output_tokens += other.reasoning_output_tokens;
        self.total_tokens += other.total_tokens;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageRecord {
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub workspace_id: Option<i64>,
    pub model: String,
    pub counts: TokenCounts,
}

/// Which usage rows a summary covers. `None` fields do not filter.
#[derive(Clone, Debug, Default)]
pub struct UsageFilter {
    pub since: Option<i64>,
    pub workspace_id: Option<i64>,
    pub thread_id: Option<String>,
    pub model: Option<String>,
}

impl Database {
    pub fn record_usage(&mut self, usage: &UsageRecord) -> Result<()> {
        let c = &usage.counts;
        self.conn.execute(
            "INSERT INTO token_usage (thread_id, turn_id, workspace_id, model, input_tokens,
                cached_input_tokens, output_tokens, reasoning_output_tokens, total_tokens, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                usage.thread_id,
                usage.turn_id,
                usage.workspace_id,
                usage.model,
                c.input_tokens,
                c.cached_input_tokens,
                c.output_tokens,
                c.reasoning_output_tokens,
                c.total_tokens,
                now_ts()
            ],
        )?;
        Ok(())
    }

    pub fn usage_total(&self, filter: &UsageFilter) -> Result<TokenCounts> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(cached_input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(reasoning_output_tokens), 0),
                    COALESCE(SUM(total_tokens), 0)
             FROM token_usage
             WHERE (?1 IS NULL OR recorded_at >= ?1)
               AND (?2 IS NULL OR workspace_id = ?2)
               AND (?3 IS NULL OR thread_id = ?3)
               AND (?4 IS NULL OR model = ?4)",
            params![filter.since, filter.workspace_id, filter.thread_id, filter.model],
            counts_from_row,
        )?)
    }

    /// Usage grouped by model, largest first.
    pub fn usage_by_model(&self, filter: &UsageFilter) -> Result<Vec<(String, TokenCounts)>> {
        let mut stmt = self.conn.prepare(
            "SELECT model, SUM(input_tokens), SUM(cached_input_tokens), SUM(output_tokens),
                    SUM(reasoning_output_tokens), SUM(total_tokens) AS total
             FROM token_usage
             WHERE (?1 IS NULL OR recorded_at >= ?1)
               AND (?2 IS NULL OR workspace_id = ?2)
               AND (?3 IS NULL OR thread_id = ?3)
             GROUP BY model ORDER BY total DESC",
        )?;
        let rows = stmt.query_map(
            params![filter.since, filter.workspace_id, filter.thread_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    TokenCounts {
                        input_tokens: row.get(1)?,
                        cached_input_tokens: row.get(2)?,
                        output_tokens: row.get(3)?,
                        reasoning_output_tokens: row.get(4)?,
                        total_tokens: row.get(5)?,
                    },
                ))
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn counts_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TokenCounts> {
    Ok(TokenCounts {
        input_tokens: row.get(0)?,
        cached_input_tokens: row.get(1)?,
        output_tokens: row.get(2)?,
        reasoning_output_tokens: row.get(3)?,
        total_tokens: row.get(4)?,
    })
}

// ---------------------------------------------------------------------------
// Budgets, policies, settings
// ---------------------------------------------------------------------------

/// A stored budget. `scope` is `global`, `workspace`, `thread` or `model`;
/// `scope_key` identifies the workspace/thread/model (empty for global).
/// `period` is `day`, `week`, `month` or `total`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetRecord {
    pub id: Option<i64>,
    pub scope: String,
    pub scope_key: String,
    pub period: String,
    pub soft_limit_tokens: Option<i64>,
    pub hard_limit_tokens: Option<i64>,
}

impl Database {
    pub fn upsert_budget(&mut self, budget: &BudgetRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO budgets (scope, scope_key, period, soft_limit_tokens, hard_limit_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope, scope_key, period) DO UPDATE SET
                soft_limit_tokens = excluded.soft_limit_tokens,
                hard_limit_tokens = excluded.hard_limit_tokens",
            params![
                budget.scope,
                budget.scope_key,
                budget.period,
                budget.soft_limit_tokens,
                budget.hard_limit_tokens
            ],
        )?;
        Ok(())
    }

    pub fn delete_budget(&mut self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM budgets WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn budgets(&self) -> Result<Vec<BudgetRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, scope, scope_key, period, soft_limit_tokens, hard_limit_tokens
             FROM budgets ORDER BY scope, scope_key, period",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(BudgetRecord {
                id: row.get(0)?,
                scope: row.get(1)?,
                scope_key: row.get(2)?,
                period: row.get(3)?,
                soft_limit_tokens: row.get(4)?,
                hard_limit_tokens: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stores an agent policy as JSON under `scope_key` (`global` or
    /// `workspace:<id>`).
    pub fn save_policy<T: Serialize>(&mut self, scope_key: &str, policy: &T) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agent_policies (scope_key, policy_json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(scope_key) DO UPDATE SET policy_json = excluded.policy_json,
                updated_at = excluded.updated_at",
            params![scope_key, serde_json::to_string(policy)?, now_ts()],
        )?;
        Ok(())
    }

    pub fn load_policy<T: for<'de> Deserialize<'de>>(&self, scope_key: &str) -> Result<Option<T>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT policy_json FROM agent_policies WHERE scope_key = ?1",
                params![scope_key],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).context("decoding stored policy"))
            .transpose()
    }

    pub fn set_setting(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", params![key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// Reads a JSON-encoded setting, falling back to `None` when it is missing
    /// or no longer decodes.
    pub fn setting_json<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        Ok(self
            .setting(key)?
            .and_then(|value| serde_json::from_str(&value).ok()))
    }

    pub fn set_setting_json<T: Serialize>(&mut self, key: &str, value: &T) -> Result<()> {
        self.set_setting(key, &serde_json::to_string(value)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn migrations_run_once_and_enable_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite3");
        {
            let db = Database::open(&path).unwrap();
            assert_eq!(db.schema_version().unwrap(), schema::MIGRATIONS.len() as i64);
            assert_eq!(db.journal_mode().unwrap(), "wal");
        }
        // Reopening must not re-run migrations (they would fail on CREATE TABLE).
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), schema::MIGRATIONS.len() as i64);
    }

    #[test]
    fn workspaces_are_upserted_and_ordered_by_recency() {
        let mut db = db();
        let a = db.touch_workspace("/src/a", "a").unwrap();
        let b = db.touch_workspace("/src/b", "b").unwrap();
        assert_ne!(a.id, b.id);
        let again = db.touch_workspace("/src/a", "a").unwrap();
        assert_eq!(again.id, a.id, "same path keeps its id");
        let recent = db.recent_workspaces(10).unwrap();
        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn tabs_round_trip_in_order() {
        let mut db = db();
        let ws = db.touch_workspace("/src/a", "a").unwrap();
        let tabs = vec![
            TabRecord { pane: 0, kind: TabKind::File, resource: "/src/a/main.rs".into(), active: false },
            TabRecord { pane: 0, kind: TabKind::Browser, resource: "http://localhost:3000".into(), active: true },
            TabRecord { pane: 1, kind: TabKind::Diff, resource: "src/lib.rs".into(), active: true },
        ];
        db.save_tabs(ws.id, &tabs).unwrap();
        assert_eq!(db.load_tabs(ws.id).unwrap(), tabs);
        // Saving again replaces rather than appends.
        db.save_tabs(ws.id, &tabs[..1]).unwrap();
        assert_eq!(db.load_tabs(ws.id).unwrap().len(), 1);
    }

    #[test]
    fn history_collapses_quick_revisits_and_searches_title_and_url() {
        let mut db = db();
        let profile = db.ensure_browser_profile("default", "/tmp/p").unwrap();
        db.record_visit(Some(profile.id), "http://localhost:3000/", Some("Dev")).unwrap();
        db.record_visit(Some(profile.id), "http://localhost:3000/", Some("Dev server")).unwrap();
        db.record_visit(Some(profile.id), "https://docs.rs/gpui", Some("gpui docs")).unwrap();

        let all = db.search_history("", 10).unwrap();
        assert_eq!(all.len(), 2);
        let dev = db.search_history("server", 10).unwrap();
        assert_eq!(dev.len(), 1);
        assert_eq!(dev[0].url, "http://localhost:3000/");
        // LIKE wildcards in the query are literal.
        assert!(db.search_history("%", 10).unwrap().is_empty());
    }

    #[test]
    fn agent_threads_and_events() {
        let mut db = db();
        let ws = db.touch_workspace("/src/a", "a").unwrap();
        let thread = AgentThreadRecord {
            thread_id: "t1".into(),
            workspace_id: Some(ws.id),
            title: Some("Refactor auth".into()),
            model: Some("gpt-test".into()),
            cwd: Some("/src/a".into()),
            created_at: 1,
            updated_at: 2,
        };
        db.upsert_agent_thread(&thread).unwrap();
        // A later upsert without a title keeps the stored one.
        db.upsert_agent_thread(&AgentThreadRecord { title: None, updated_at: 3, ..thread.clone() })
            .unwrap();
        let threads = db.agent_threads(ws.id, false).unwrap();
        assert_eq!(threads[0].title.as_deref(), Some("Refactor auth"));
        assert_eq!(threads[0].updated_at, 3);

        let first = db
            .append_agent_event("t1", Some("turn1"), "turn/started", &serde_json::json!({"a": 1}))
            .unwrap();
        db.append_agent_event("t1", Some("turn1"), "turn/completed", &serde_json::json!({}))
            .unwrap();
        let events = db.agent_events("t1", 0, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["a"], 1);
        assert_eq!(db.agent_events("t1", first, 10).unwrap().len(), 1);

        db.archive_agent_thread("t1").unwrap();
        assert!(db.agent_threads(ws.id, false).unwrap().is_empty());
        assert_eq!(db.agent_threads(ws.id, true).unwrap().len(), 1);
    }

    #[test]
    fn usage_sums_with_filters() {
        let mut db = db();
        let counts = TokenCounts {
            input_tokens: 100,
            cached_input_tokens: 10,
            output_tokens: 50,
            reasoning_output_tokens: 5,
            total_tokens: 150,
        };
        for (thread, model) in [("t1", "m1"), ("t1", "m1"), ("t2", "m2")] {
            db.record_usage(&UsageRecord {
                thread_id: Some(thread.into()),
                turn_id: None,
                workspace_id: Some(7),
                model: model.into(),
                counts,
            })
            .unwrap();
        }
        let all = db.usage_total(&UsageFilter::default()).unwrap();
        assert_eq!(all.total_tokens, 450);
        let t1 = db
            .usage_total(&UsageFilter { thread_id: Some("t1".into()), ..Default::default() })
            .unwrap();
        assert_eq!(t1.total_tokens, 300);
        let other_ws = db
            .usage_total(&UsageFilter { workspace_id: Some(8), ..Default::default() })
            .unwrap();
        assert_eq!(other_ws, TokenCounts::default());
        let future = db
            .usage_total(&UsageFilter { since: Some(now_ts() + 60), ..Default::default() })
            .unwrap();
        assert_eq!(future.total_tokens, 0);
        let by_model = db.usage_by_model(&UsageFilter::default()).unwrap();
        assert_eq!(by_model[0].0, "m1");
        assert_eq!(by_model[0].1.total_tokens, 300);
    }

    #[test]
    fn budgets_policies_and_settings() {
        let mut db = db();
        let budget = BudgetRecord {
            id: None,
            scope: "workspace".into(),
            scope_key: "1".into(),
            period: "day".into(),
            soft_limit_tokens: Some(1000),
            hard_limit_tokens: Some(2000),
        };
        db.upsert_budget(&budget).unwrap();
        db.upsert_budget(&BudgetRecord { hard_limit_tokens: Some(3000), ..budget.clone() })
            .unwrap();
        let budgets = db.budgets().unwrap();
        assert_eq!(budgets.len(), 1);
        assert_eq!(budgets[0].hard_limit_tokens, Some(3000));

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Policy {
            max_chars: usize,
        }
        db.save_policy("global", &Policy { max_chars: 10 }).unwrap();
        assert_eq!(db.load_policy::<Policy>("global").unwrap(), Some(Policy { max_chars: 10 }));
        assert_eq!(db.load_policy::<Policy>("workspace:1").unwrap(), None);

        db.set_setting("theme", "dark").unwrap();
        db.set_setting("theme", "light").unwrap();
        assert_eq!(db.setting("theme").unwrap().as_deref(), Some("light"));
        db.set_setting_json("recent", &vec![1, 2]).unwrap();
        assert_eq!(db.setting_json::<Vec<i32>>("recent").unwrap(), Some(vec![1, 2]));
        // A value that no longer decodes is treated as unset.
        assert_eq!(db.setting_json::<Vec<i32>>("theme").unwrap(), None);
    }
}
