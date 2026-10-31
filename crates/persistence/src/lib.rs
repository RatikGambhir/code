//! Hangar's application database.
//!
//! [`Database`] owns one SQLite connection in WAL mode. All access goes
//! through two entry points:
//!
//! - [`Database::read`] hands a [`Reader`] to a closure, inside a deferred
//!   transaction so multi-statement reads see one consistent snapshot.
//! - [`Database::write`] hands a [`Writer`] to a closure, inside a
//!   `BEGIN IMMEDIATE` transaction that commits when the closure returns
//!   `Ok` and rolls back when it returns `Err` or panics.
//!
//! Write operations exist only on [`Writer`], so a write cannot happen
//! outside a transaction, and several writes in one closure are atomic as a
//! group. Queries are built with the typed builder in [`sql`].
//!
//! [`DbHandle`] moves the database onto a dedicated thread so the GPUI render
//! loop never waits on disk.

pub mod schema;
mod service;
pub mod sql;

use std::{ops::Deref, path::Path};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, Row, TransactionBehavior};
use serde::{Deserialize, Serialize};

pub use schema::{BudgetPeriod, BudgetScope, TabKind};
pub use service::DbHandle;
use schema::*;
use sql::{Column, Condition, Expr, Order, RowExt};

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

/// Read access, valid for the duration of one [`Database::read`] or
/// [`Database::write`] call.
pub struct Reader<'c> {
    conn: &'c Connection,
}

/// Write access, valid inside one [`Database::write`] transaction. Derefs to
/// [`Reader`], so reads inside a write see the transaction's own changes.
pub struct Writer<'c> {
    reader: Reader<'c>,
}

impl<'c> Reader<'c> {
    pub(crate) fn connection(&self) -> &'c Connection {
        self.conn
    }
}

impl<'c> Writer<'c> {
    pub(crate) fn connection(&self) -> &'c Connection {
        self.reader.conn
    }
}

impl<'c> Deref for Writer<'c> {
    type Target = Reader<'c>;

    fn deref(&self) -> &Reader<'c> {
        &self.reader
    }
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
        // WAL lets readers proceed while a write transaction is open; NORMAL
        // sync is durable across app crashes, which is what matters here.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        let version = self.schema_version()? as usize;
        if version > schema::MIGRATIONS.len() {
            anyhow::bail!(
                "database schema version {version} is newer than this build supports ({})",
                schema::MIGRATIONS.len()
            );
        }
        for (index, migration) in schema::MIGRATIONS.iter().enumerate().skip(version) {
            let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(migration)
                .with_context(|| format!("applying migration {}", index + 1))?;
            tx.pragma_update(None, "user_version", (index + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Runs `f` with read access, in a deferred transaction.
    pub fn read<T>(&mut self, f: impl FnOnce(&Reader<'_>) -> Result<T>) -> Result<T> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let value = f(&Reader { conn: &tx })?;
        tx.commit()?;
        Ok(value)
    }

    /// Runs `f` with write access, atomically: all of its writes commit
    /// together when it returns `Ok`, and none do if it returns `Err` or
    /// panics. `IMMEDIATE` takes the write lock up front, so the transaction
    /// can never fail half-way with `SQLITE_BUSY` on its first write.
    pub fn write<T>(&mut self, f: impl FnOnce(&Writer<'_>) -> Result<T>) -> Result<T> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&Writer {
            reader: Reader { conn: &tx },
        })?;
        tx.commit().context("committing the transaction")?;
        Ok(value)
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

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
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

impl WorkspaceRecord {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.col(Workspaces::Id)?,
            path: row.col(Workspaces::Path)?,
            name: row.col(Workspaces::Name)?,
            last_opened: row.col(Workspaces::LastOpened)?,
            layout_json: row.col(Workspaces::LayoutJson)?,
        })
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

impl Reader<'_> {
    pub fn workspace_by_path(&self, path: &str) -> Result<Option<WorkspaceRecord>> {
        Workspaces::select(Workspaces::ALL.iter().copied())
            .filter(Workspaces::Path.equals(path))
            .fetch_optional(self, WorkspaceRecord::from_row)
    }

    pub fn recent_workspaces(&self, limit: usize) -> Result<Vec<WorkspaceRecord>> {
        Workspaces::select(Workspaces::ALL.iter().copied())
            .order_by(Workspaces::LastOpened, Order::Desc)
            .limit(limit)
            .fetch_all(self, WorkspaceRecord::from_row)
    }

    pub fn load_tabs(&self, workspace_id: i64) -> Result<Vec<TabRecord>> {
        Tabs::select([Tabs::Pane, Tabs::Kind, Tabs::Resource, Tabs::Active])
            .filter(Tabs::WorkspaceId.equals(workspace_id))
            .order_by(Tabs::Pane, Order::Asc)
            .order_by(Tabs::Position, Order::Asc)
            .fetch_all(self, |row| {
                Ok(TabRecord {
                    pane: row.col(Tabs::Pane)?,
                    kind: row.col(Tabs::Kind)?,
                    resource: row.col(Tabs::Resource)?,
                    active: row.col(Tabs::Active)?,
                })
            })
    }
}

impl Writer<'_> {
    /// Records that `path` was opened now, creating the workspace on first use.
    pub fn touch_workspace(&self, path: &str, name: &str) -> Result<WorkspaceRecord> {
        Workspaces::insert()
            .value(Workspaces::Path, path)
            .value(Workspaces::Name, name)
            .value(Workspaces::LastOpened, now_ts())
            .on_conflict([Workspaces::Path])
            .take_new(Workspaces::Name)
            .take_new(Workspaces::LastOpened)
            .execute(self)?;
        self.workspace_by_path(path)?
            .context("workspace vanished after upsert")
    }

    pub fn save_layout(&self, workspace_id: i64, layout_json: &str) -> Result<()> {
        Workspaces::update()
            .set(Workspaces::LayoutJson, layout_json)
            .filter(Workspaces::Id.equals(workspace_id))
            .execute(self)?;
        Ok(())
    }

    /// Replaces the workspace's open tabs. Runs inside the caller's
    /// transaction, so the delete and the inserts land together or not at all.
    pub fn save_tabs(&self, workspace_id: i64, tabs: &[TabRecord]) -> Result<()> {
        Tabs::delete()
            .filter(Tabs::WorkspaceId.equals(workspace_id))
            .execute(self)?;
        for (position, tab) in tabs.iter().enumerate() {
            Tabs::insert()
                .value(Tabs::WorkspaceId, workspace_id)
                .value(Tabs::Pane, tab.pane)
                .value(Tabs::Kind, tab.kind)
                .value(Tabs::Resource, &tab.resource)
                .value(Tabs::Position, position)
                .value(Tabs::Active, tab.active)
                .execute(self)?;
        }
        Ok(())
    }
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

/// Visits to the same URL within this many seconds count as one.
const VISIT_COLLAPSE_SECS: i64 = 10;

impl Reader<'_> {
    pub fn browser_profile(&self, name: &str) -> Result<Option<BrowserProfileRecord>> {
        BrowserProfiles::select([BrowserProfiles::Id, BrowserProfiles::Name, BrowserProfiles::StorageDir])
            .filter(BrowserProfiles::Name.equals(name))
            .fetch_optional(self, |row| {
                Ok(BrowserProfileRecord {
                    id: row.col(BrowserProfiles::Id)?,
                    name: row.col(BrowserProfiles::Name)?,
                    storage_dir: row.col(BrowserProfiles::StorageDir)?,
                })
            })
    }

    /// Most recent distinct URLs whose URL or title contains `query`.
    pub fn search_history(&self, query: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        let matches = (!query.is_empty())
            .then(|| BrowserHistory::Url.contains(query).or(BrowserHistory::Title.contains(query)));
        // With MAX(), SQLite takes the bare `title` from the latest visit.
        BrowserHistory::select([
            Expr::from(BrowserHistory::Url),
            Expr::from(BrowserHistory::Title),
            BrowserHistory::VisitedAt.max().aliased_as(BrowserHistory::VisitedAt),
        ])
        .filter_if(matches)
        .group_by(BrowserHistory::Url)
        .order_by(BrowserHistory::VisitedAt.max(), Order::Desc)
        .limit(limit)
        .fetch_all(self, |row| {
            Ok(HistoryEntry {
                url: row.col(BrowserHistory::Url)?,
                title: row.col(BrowserHistory::Title)?,
                visited_at: row.col(BrowserHistory::VisitedAt)?,
            })
        })
    }
}

impl Writer<'_> {
    pub fn ensure_browser_profile(&self, name: &str, storage_dir: &str) -> Result<BrowserProfileRecord> {
        BrowserProfiles::insert()
            .value(BrowserProfiles::Name, name)
            .value(BrowserProfiles::StorageDir, storage_dir)
            .value(BrowserProfiles::CreatedAt, now_ts())
            .on_conflict_do_nothing([BrowserProfiles::Name])
            .execute(self)?;
        self.browser_profile(name)?
            .context("browser profile vanished after insert")
    }

    /// Records a page visit. Reloads and redirects that land on the same URL
    /// within a few seconds update the latest entry instead of adding one.
    pub fn record_visit(&self, profile_id: Option<i64>, url: &str, title: Option<&str>) -> Result<()> {
        let now = now_ts();
        let recent: Option<i64> = BrowserHistory::select([BrowserHistory::Id])
            .filter(BrowserHistory::Url.equals(url))
            .filter(BrowserHistory::ProfileId.is(profile_id))
            .filter(BrowserHistory::VisitedAt.at_least(now - VISIT_COLLAPSE_SECS))
            .order_by(BrowserHistory::VisitedAt, Order::Desc)
            .fetch_optional(self, |row| row.col(BrowserHistory::Id))?;

        match recent {
            Some(id) => {
                let mut update = BrowserHistory::update()
                    .set(BrowserHistory::VisitedAt, now)
                    .filter(BrowserHistory::Id.equals(id));
                if let Some(title) = title {
                    update = update.set(BrowserHistory::Title, title);
                }
                update.execute(self)?;
            }
            None => {
                BrowserHistory::insert()
                    .value(BrowserHistory::ProfileId, profile_id)
                    .value(BrowserHistory::Url, url)
                    .value(BrowserHistory::Title, title)
                    .value(BrowserHistory::VisitedAt, now)
                    .execute(self)?;
            }
        }
        Ok(())
    }

    pub fn clear_history(&self) -> Result<()> {
        BrowserHistory::delete().all_rows().execute(self)?;
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
    /// The agent provider ("openai", "anthropic"); `None` for older threads,
    /// which are Codex's.
    pub provider: Option<String>,
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

impl Reader<'_> {
    pub fn agent_threads(&self, workspace_id: i64, include_archived: bool) -> Result<Vec<AgentThreadRecord>> {
        AgentThreads::select([
            AgentThreads::ThreadId,
            AgentThreads::WorkspaceId,
            AgentThreads::Title,
            AgentThreads::Model,
            AgentThreads::Cwd,
            AgentThreads::CreatedAt,
            AgentThreads::UpdatedAt,
            AgentThreads::Provider,
        ])
        .filter(AgentThreads::WorkspaceId.equals(workspace_id))
        .filter_if((!include_archived).then(|| AgentThreads::Archived.equals(false)))
        .order_by(AgentThreads::UpdatedAt, Order::Desc)
        .fetch_all(self, |row| {
            Ok(AgentThreadRecord {
                thread_id: row.col(AgentThreads::ThreadId)?,
                workspace_id: row.col(AgentThreads::WorkspaceId)?,
                title: row.col(AgentThreads::Title)?,
                model: row.col(AgentThreads::Model)?,
                cwd: row.col(AgentThreads::Cwd)?,
                created_at: row.col(AgentThreads::CreatedAt)?,
                updated_at: row.col(AgentThreads::UpdatedAt)?,
                provider: row.col(AgentThreads::Provider)?,
            })
        })
    }

    pub fn agent_events(&self, thread_id: &str, after_id: i64, limit: usize) -> Result<Vec<AgentEventRecord>> {
        AgentEvents::select(AgentEvents::ALL.iter().copied())
            .filter(AgentEvents::ThreadId.equals(thread_id))
            .filter(AgentEvents::Id.greater_than(after_id))
            .order_by(AgentEvents::Id, Order::Asc)
            .limit(limit)
            .fetch_all(self, |row| {
                let payload: String = row.col(AgentEvents::Payload)?;
                Ok(AgentEventRecord {
                    id: row.col(AgentEvents::Id)?,
                    thread_id: row.col(AgentEvents::ThreadId)?,
                    turn_id: row.col(AgentEvents::TurnId)?,
                    event_type: row.col(AgentEvents::EventType)?,
                    payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                    created_at: row.col(AgentEvents::CreatedAt)?,
                })
            })
    }
}

impl Writer<'_> {
    /// Inserts or refreshes a thread. Title, model, cwd and provider are only
    /// replaced by non-null values, so a partial update never erases them.
    pub fn upsert_agent_thread(&self, thread: &AgentThreadRecord) -> Result<()> {
        AgentThreads::insert()
            .value(AgentThreads::ThreadId, &thread.thread_id)
            .value(AgentThreads::WorkspaceId, thread.workspace_id)
            .value(AgentThreads::Title, thread.title.as_deref())
            .value(AgentThreads::Model, thread.model.as_deref())
            .value(AgentThreads::Cwd, thread.cwd.as_deref())
            .value(AgentThreads::CreatedAt, thread.created_at)
            .value(AgentThreads::UpdatedAt, thread.updated_at)
            .value(AgentThreads::Provider, thread.provider.as_deref())
            .on_conflict([AgentThreads::ThreadId])
            .keep_existing_if_null(AgentThreads::Title)
            .keep_existing_if_null(AgentThreads::Model)
            .keep_existing_if_null(AgentThreads::Cwd)
            .keep_existing_if_null(AgentThreads::Provider)
            .take_new(AgentThreads::UpdatedAt)
            .execute(self)?;
        Ok(())
    }

    pub fn touch_agent_thread(&self, thread_id: &str) -> Result<()> {
        AgentThreads::update()
            .set(AgentThreads::UpdatedAt, now_ts())
            .filter(AgentThreads::ThreadId.equals(thread_id))
            .execute(self)?;
        Ok(())
    }

    pub fn rename_agent_thread(&self, thread_id: &str, title: &str) -> Result<()> {
        AgentThreads::update()
            .set(AgentThreads::Title, title)
            .filter(AgentThreads::ThreadId.equals(thread_id))
            .execute(self)?;
        Ok(())
    }

    pub fn archive_agent_thread(&self, thread_id: &str) -> Result<()> {
        AgentThreads::update()
            .set(AgentThreads::Archived, true)
            .filter(AgentThreads::ThreadId.equals(thread_id))
            .execute(self)?;
        Ok(())
    }

    pub fn append_agent_event(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<i64> {
        AgentEvents::insert()
            .value(AgentEvents::ThreadId, thread_id)
            .value(AgentEvents::TurnId, turn_id)
            .value(AgentEvents::EventType, event_type)
            .value(AgentEvents::Payload, payload.to_string())
            .value(AgentEvents::CreatedAt, now_ts())
            .execute_returning_id(self)
    }

    /// Drops events older than `before_ts`, keeping the log bounded.
    pub fn prune_agent_events(&self, before_ts: i64) -> Result<usize> {
        AgentEvents::delete()
            .filter(AgentEvents::CreatedAt.less_than(before_ts))
            .execute(self)
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
    /// Component-wise `self - earlier`, never negative (a counter that went
    /// backwards, e.g. after a thread was compacted, contributes nothing).
    pub fn saturating_sub(&self, earlier: &TokenCounts) -> TokenCounts {
        TokenCounts {
            input_tokens: (self.input_tokens - earlier.input_tokens).max(0),
            cached_input_tokens: (self.cached_input_tokens - earlier.cached_input_tokens).max(0),
            output_tokens: (self.output_tokens - earlier.output_tokens).max(0),
            reasoning_output_tokens: (self.reasoning_output_tokens - earlier.reasoning_output_tokens)
                .max(0),
            total_tokens: (self.total_tokens - earlier.total_tokens).max(0),
        }
    }

    pub fn add(&mut self, other: &TokenCounts) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_output_tokens += other.reasoning_output_tokens;
        self.total_tokens += other.total_tokens;
    }

    /// The summed counter columns, named after the columns themselves.
    fn sums() -> [Expr; 5] {
        [
            TokenUsage::InputTokens.sum_or_zero().aliased_as(TokenUsage::InputTokens),
            TokenUsage::CachedInputTokens.sum_or_zero().aliased_as(TokenUsage::CachedInputTokens),
            TokenUsage::OutputTokens.sum_or_zero().aliased_as(TokenUsage::OutputTokens),
            TokenUsage::ReasoningOutputTokens
                .sum_or_zero()
                .aliased_as(TokenUsage::ReasoningOutputTokens),
            TokenUsage::TotalTokens.sum_or_zero().aliased_as(TokenUsage::TotalTokens),
        ]
    }

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            input_tokens: row.col(TokenUsage::InputTokens)?,
            cached_input_tokens: row.col(TokenUsage::CachedInputTokens)?,
            output_tokens: row.col(TokenUsage::OutputTokens)?,
            reasoning_output_tokens: row.col(TokenUsage::ReasoningOutputTokens)?,
            total_tokens: row.col(TokenUsage::TotalTokens)?,
        })
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

impl UsageFilter {
    fn conditions(&self) -> impl Iterator<Item = Condition> {
        [
            self.since.map(|since| TokenUsage::RecordedAt.at_least(since)),
            self.workspace_id.map(|id| TokenUsage::WorkspaceId.equals(id)),
            self.thread_id.clone().map(|id| TokenUsage::ThreadId.equals(id)),
            self.model.clone().map(|model| TokenUsage::Model.equals(model)),
        ]
        .into_iter()
        .flatten()
    }
}

impl Reader<'_> {
    pub fn usage_total(&self, filter: &UsageFilter) -> Result<TokenCounts> {
        filter
            .conditions()
            .fold(TokenUsage::select(TokenCounts::sums()), |query, condition| {
                query.filter(condition)
            })
            .fetch_one(self, TokenCounts::from_row)
    }

    /// Usage grouped by model, largest first.
    pub fn usage_by_model(&self, filter: &UsageFilter) -> Result<Vec<(String, TokenCounts)>> {
        filter
            .conditions()
            .fold(
                TokenUsage::select(TokenCounts::sums()).and_select(TokenUsage::Model),
                |query, condition| query.filter(condition),
            )
            .group_by(TokenUsage::Model)
            .order_by(TokenUsage::TotalTokens.sum(), Order::Desc)
            .fetch_all(self, |row| Ok((row.col(TokenUsage::Model)?, TokenCounts::from_row(row)?)))
    }
}

impl Writer<'_> {
    /// Records the usage implied by a new cumulative total for a thread.
    ///
    /// Codex reports running totals per thread. This compares `total` with
    /// the baseline stored for the thread, records the difference as one
    /// usage row and moves the baseline forward — in the caller's
    /// transaction, so the row and the baseline can never disagree. Returns
    /// the recorded delta (all zeros when nothing new was used).
    pub fn record_thread_usage_total(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        workspace_id: Option<i64>,
        model: &str,
        total: TokenCounts,
    ) -> Result<TokenCounts> {
        let stored: Option<Option<String>> = AgentThreads::select([AgentThreads::UsageBaseline])
            .filter(AgentThreads::ThreadId.equals(thread_id))
            .fetch_optional(self, |row| row.col(AgentThreads::UsageBaseline))?;
        if stored.is_none() {
            let now = now_ts();
            self.upsert_agent_thread(&AgentThreadRecord {
                thread_id: thread_id.into(),
                workspace_id,
                title: None,
                model: Some(model.into()),
                cwd: None,
                created_at: now,
                updated_at: now,
                provider: None,
            })?;
        }
        let baseline: TokenCounts = stored
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();

        let delta = total.saturating_sub(&baseline);
        if delta.total_tokens > 0 || delta.input_tokens > 0 || delta.output_tokens > 0 {
            self.record_usage(&UsageRecord {
                thread_id: Some(thread_id.into()),
                turn_id: turn_id.map(str::to_string),
                workspace_id,
                model: model.into(),
                counts: delta,
            })?;
        }
        AgentThreads::update()
            .set(AgentThreads::UsageBaseline, serde_json::to_string(&total)?)
            .filter(AgentThreads::ThreadId.equals(thread_id))
            .execute(self)?;
        Ok(delta)
    }

    pub fn record_usage(&self, usage: &UsageRecord) -> Result<()> {
        let counts = &usage.counts;
        TokenUsage::insert()
            .value(TokenUsage::ThreadId, usage.thread_id.as_deref())
            .value(TokenUsage::TurnId, usage.turn_id.as_deref())
            .value(TokenUsage::WorkspaceId, usage.workspace_id)
            .value(TokenUsage::Model, &usage.model)
            .value(TokenUsage::InputTokens, counts.input_tokens)
            .value(TokenUsage::CachedInputTokens, counts.cached_input_tokens)
            .value(TokenUsage::OutputTokens, counts.output_tokens)
            .value(TokenUsage::ReasoningOutputTokens, counts.reasoning_output_tokens)
            .value(TokenUsage::TotalTokens, counts.total_tokens)
            .value(TokenUsage::RecordedAt, now_ts())
            .execute(self)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Budgets, policies, settings
// ---------------------------------------------------------------------------

/// A stored token budget.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetRecord {
    pub id: Option<i64>,
    pub scope: BudgetScope,
    /// The workspace id, thread id or model the budget applies to; empty for
    /// [`BudgetScope::Global`].
    pub scope_key: String,
    pub period: BudgetPeriod,
    pub soft_limit_tokens: Option<i64>,
    pub hard_limit_tokens: Option<i64>,
}

impl Reader<'_> {
    pub fn budgets(&self) -> Result<Vec<BudgetRecord>> {
        Budgets::select(Budgets::ALL.iter().copied())
            .order_by(Budgets::Scope, Order::Asc)
            .order_by(Budgets::ScopeKey, Order::Asc)
            .order_by(Budgets::Period, Order::Asc)
            .fetch_all(self, |row| {
                Ok(BudgetRecord {
                    id: row.col(Budgets::Id)?,
                    scope: row.col(Budgets::Scope)?,
                    scope_key: row.col(Budgets::ScopeKey)?,
                    period: row.col(Budgets::Period)?,
                    soft_limit_tokens: row.col(Budgets::SoftLimitTokens)?,
                    hard_limit_tokens: row.col(Budgets::HardLimitTokens)?,
                })
            })
    }

    pub fn load_policy<T: for<'de> Deserialize<'de>>(&self, scope_key: &str) -> Result<Option<T>> {
        let json: Option<String> = AgentPolicies::select([AgentPolicies::PolicyJson])
            .filter(AgentPolicies::ScopeKey.equals(scope_key))
            .fetch_optional(self, |row| row.col(AgentPolicies::PolicyJson))?;
        json.map(|json| serde_json::from_str(&json).context("decoding stored policy"))
            .transpose()
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Settings::select([Settings::Value])
            .filter(Settings::Key.equals(key))
            .fetch_optional(self, |row| row.col(Settings::Value))
    }

    /// Reads a JSON-encoded setting; a missing value or one that no longer
    /// decodes is `None`.
    pub fn setting_json<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        Ok(self
            .setting(key)?
            .and_then(|value| serde_json::from_str(&value).ok()))
    }
}

impl Writer<'_> {
    pub fn upsert_budget(&self, budget: &BudgetRecord) -> Result<()> {
        Budgets::insert()
            .value(Budgets::Scope, budget.scope)
            .value(Budgets::ScopeKey, &budget.scope_key)
            .value(Budgets::Period, budget.period)
            .value(Budgets::SoftLimitTokens, budget.soft_limit_tokens)
            .value(Budgets::HardLimitTokens, budget.hard_limit_tokens)
            .on_conflict([Budgets::Scope, Budgets::ScopeKey, Budgets::Period])
            .take_new(Budgets::SoftLimitTokens)
            .take_new(Budgets::HardLimitTokens)
            .execute(self)?;
        Ok(())
    }

    pub fn delete_budget(&self, id: i64) -> Result<()> {
        Budgets::delete().filter(Budgets::Id.equals(id)).execute(self)?;
        Ok(())
    }

    /// Stores an agent policy as JSON under `scope_key` (`global` or
    /// `workspace:<id>`).
    pub fn save_policy<T: Serialize>(&self, scope_key: &str, policy: &T) -> Result<()> {
        AgentPolicies::insert()
            .value(AgentPolicies::ScopeKey, scope_key)
            .value(AgentPolicies::PolicyJson, serde_json::to_string(policy)?)
            .value(AgentPolicies::UpdatedAt, now_ts())
            .on_conflict([AgentPolicies::ScopeKey])
            .take_new(AgentPolicies::PolicyJson)
            .take_new(AgentPolicies::UpdatedAt)
            .execute(self)?;
        Ok(())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        Settings::insert()
            .value(Settings::Key, key)
            .value(Settings::Value, value)
            .on_conflict([Settings::Key])
            .take_new(Settings::Value)
            .execute(self)?;
        Ok(())
    }

    pub fn set_setting_json<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.set_setting(key, &serde_json::to_string(value)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn tab(pane: i64, kind: TabKind, resource: &str, active: bool) -> TabRecord {
        TabRecord {
            pane,
            kind,
            resource: resource.into(),
            active,
        }
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
    fn a_failed_write_rolls_back_everything_it_did() {
        let mut db = db();
        let result: Result<()> = db.write(|w| {
            w.touch_workspace("/src/a", "a")?;
            w.set_setting("theme", "dark")?;
            anyhow::bail!("something went wrong after two writes")
        });
        assert!(result.is_err());
        db.read(|r| {
            assert!(r.workspace_by_path("/src/a")?.is_none());
            assert!(r.setting("theme")?.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_panicking_write_rolls_back() {
        let mut db = db();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = db.write(|w| -> Result<()> {
                w.set_setting("k", "v")?;
                panic!("bug in the middle of a transaction");
            });
        }));
        assert!(outcome.is_err());
        assert_eq!(db.read(|r| r.setting("k")).unwrap(), None);
    }

    #[test]
    fn replacing_tabs_is_all_or_nothing() {
        let mut db = db();
        let ws = db.write(|w| w.touch_workspace("/src/a", "a")).unwrap();
        let original = vec![tab(0, TabKind::File, "/src/a/main.rs", true)];
        db.write(|w| w.save_tabs(ws.id, &original)).unwrap();

        // Saving for a workspace that does not exist violates the foreign
        // key on the first insert, after the delete already ran for `ws`.
        let result = db.write(|w| {
            w.save_tabs(ws.id, &[tab(0, TabKind::Browser, "http://localhost:3000", true)])?;
            w.save_tabs(9999, &[tab(0, TabKind::File, "/nowhere", true)])
        });
        assert!(result.is_err());
        assert_eq!(db.read(|r| r.load_tabs(ws.id)).unwrap(), original);
    }

    #[test]
    fn workspaces_are_upserted_and_ordered_by_recency() {
        let mut db = db();
        let (a, b, again) = db
            .write(|w| {
                Ok((
                    w.touch_workspace("/src/a", "a")?,
                    w.touch_workspace("/src/b", "b")?,
                    w.touch_workspace("/src/a", "renamed")?,
                ))
            })
            .unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(again.id, a.id, "same path keeps its id");
        assert_eq!(again.name, "renamed");
        assert_eq!(db.read(|r| r.recent_workspaces(10)).unwrap().len(), 2);
    }

    #[test]
    fn tabs_round_trip_in_order() {
        let mut db = db();
        let ws = db.write(|w| w.touch_workspace("/src/a", "a")).unwrap();
        let tabs = vec![
            tab(0, TabKind::File, "/src/a/main.rs", false),
            tab(0, TabKind::Browser, "http://localhost:3000", true),
            tab(1, TabKind::Diff, "src/lib.rs", true),
        ];
        db.write(|w| w.save_tabs(ws.id, &tabs)).unwrap();
        assert_eq!(db.read(|r| r.load_tabs(ws.id)).unwrap(), tabs);
        db.write(|w| w.save_tabs(ws.id, &tabs[..1])).unwrap();
        assert_eq!(db.read(|r| r.load_tabs(ws.id)).unwrap().len(), 1);
    }

    #[test]
    fn history_collapses_quick_revisits_and_searches_title_and_url() {
        let mut db = db();
        db.write(|w| {
            let profile = w.ensure_browser_profile("default", "/tmp/p")?;
            assert_eq!(w.ensure_browser_profile("default", "/elsewhere")?.id, profile.id);
            w.record_visit(Some(profile.id), "http://localhost:3000/", Some("Dev"))?;
            w.record_visit(Some(profile.id), "http://localhost:3000/", Some("Dev server"))?;
            w.record_visit(Some(profile.id), "https://docs.rs/gpui", Some("gpui docs"))?;
            w.record_visit(None, "https://example.com/", None)
        })
        .unwrap();

        db.read(|r| {
            assert_eq!(r.search_history("", 10)?.len(), 3);
            let dev = r.search_history("SERVER", 10)?;
            assert_eq!(dev.len(), 1);
            assert_eq!(dev[0].url, "http://localhost:3000/");
            assert_eq!(dev[0].title.as_deref(), Some("Dev server"));
            // LIKE wildcards in the query are literal.
            assert!(r.search_history("%", 10)?.is_empty());
            Ok(())
        })
        .unwrap();

        db.write(|w| w.clear_history()).unwrap();
        assert!(db.read(|r| r.search_history("", 10)).unwrap().is_empty());
    }

    #[test]
    fn agent_threads_and_events() {
        let mut db = db();
        let ws = db.write(|w| w.touch_workspace("/src/a", "a")).unwrap();
        let thread = AgentThreadRecord {
            thread_id: "t1".into(),
            workspace_id: Some(ws.id),
            title: Some("Refactor auth".into()),
            model: Some("gpt-test".into()),
            cwd: Some("/src/a".into()),
            created_at: 1,
            updated_at: 2,
            provider: Some("anthropic".into()),
        };
        db.write(|w| {
            w.upsert_agent_thread(&thread)?;
            // A later upsert without a title keeps the stored one.
            w.upsert_agent_thread(&AgentThreadRecord {
                title: None,
                updated_at: 3,
                ..thread.clone()
            })
        })
        .unwrap();
        let threads = db.read(|r| r.agent_threads(ws.id, false)).unwrap();
        assert_eq!(threads[0].title.as_deref(), Some("Refactor auth"));
        assert_eq!(threads[0].provider.as_deref(), Some("anthropic"), "kept by a partial upsert");
        assert_eq!(threads[0].updated_at, 3);

        let first = db
            .write(|w| {
                let first = w.append_agent_event("t1", Some("turn1"), "turn/started", &serde_json::json!({"a": 1}))?;
                w.append_agent_event("t1", Some("turn1"), "turn/completed", &serde_json::json!({}))?;
                Ok(first)
            })
            .unwrap();
        let events = db.read(|r| r.agent_events("t1", 0, 10)).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["a"], 1);
        assert_eq!(db.read(|r| r.agent_events("t1", first, 10)).unwrap().len(), 1);
        assert_eq!(db.write(|w| w.prune_agent_events(now_ts() + 1)).unwrap(), 2);

        db.write(|w| w.archive_agent_thread("t1")).unwrap();
        assert!(db.read(|r| r.agent_threads(ws.id, false)).unwrap().is_empty());
        assert_eq!(db.read(|r| r.agent_threads(ws.id, true)).unwrap().len(), 1);
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
        db.write(|w| {
            for (thread, model) in [("t1", "m1"), ("t1", "m1"), ("t2", "m2")] {
                w.record_usage(&UsageRecord {
                    thread_id: Some(thread.into()),
                    turn_id: None,
                    workspace_id: Some(7),
                    model: model.into(),
                    counts,
                })?;
            }
            Ok(())
        })
        .unwrap();

        db.read(|r| {
            assert_eq!(r.usage_total(&UsageFilter::default())?.total_tokens, 450);
            let t1 = r.usage_total(&UsageFilter { thread_id: Some("t1".into()), ..Default::default() })?;
            assert_eq!(t1.total_tokens, 300);
            assert_eq!(t1.cached_input_tokens, 20);
            let other_ws = r.usage_total(&UsageFilter { workspace_id: Some(8), ..Default::default() })?;
            assert_eq!(other_ws, TokenCounts::default());
            let future = r.usage_total(&UsageFilter { since: Some(now_ts() + 60), ..Default::default() })?;
            assert_eq!(future.total_tokens, 0);
            let by_model = r.usage_by_model(&UsageFilter::default())?;
            assert_eq!(by_model[0].0, "m1");
            assert_eq!(by_model[0].1.total_tokens, 300);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn cumulative_totals_become_deltas_without_double_counting() {
        let mut db = db();
        let total = |n: i64| TokenCounts {
            input_tokens: n * 3,
            output_tokens: n,
            total_tokens: n * 4,
            ..Default::default()
        };
        let ws = db.write(|w| w.touch_workspace("/src/a", "a")).unwrap().id;
        db.write(|w| {
            // First report for an unknown thread creates it.
            assert_eq!(w.record_thread_usage_total("t1", Some("u1"), Some(ws), "m", total(10))?.total_tokens, 40);
            assert_eq!(w.record_thread_usage_total("t1", Some("u2"), Some(ws), "m", total(15))?.total_tokens, 20);
            // A repeated total (e.g. re-sent after resume) records nothing.
            assert_eq!(w.record_thread_usage_total("t1", Some("u2"), Some(ws), "m", total(15))?, TokenCounts::default());
            Ok(())
        })
        .unwrap();
        let recorded = db
            .read(|r| r.usage_total(&UsageFilter { thread_id: Some("t1".into()), ..Default::default() }))
            .unwrap();
        assert_eq!(recorded.total_tokens, 60);
        assert_eq!(recorded.input_tokens, 45);
        // The thread row the first report created is a normal thread.
        assert_eq!(db.read(|r| r.agent_threads(ws, false)).unwrap().len(), 1);
    }

    #[test]
    fn budgets_policies_and_settings() {
        let mut db = db();
        let budget = BudgetRecord {
            id: None,
            scope: BudgetScope::Workspace,
            scope_key: "1".into(),
            period: BudgetPeriod::Day,
            soft_limit_tokens: Some(1000),
            hard_limit_tokens: Some(2000),
        };
        db.write(|w| {
            w.upsert_budget(&budget)?;
            w.upsert_budget(&BudgetRecord { hard_limit_tokens: Some(3000), ..budget.clone() })
        })
        .unwrap();
        let budgets = db.read(|r| r.budgets()).unwrap();
        assert_eq!(budgets.len(), 1);
        assert_eq!(budgets[0].scope, BudgetScope::Workspace);
        assert_eq!(budgets[0].hard_limit_tokens, Some(3000));
        db.write(|w| w.delete_budget(budgets[0].id.unwrap())).unwrap();
        assert!(db.read(|r| r.budgets()).unwrap().is_empty());

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Policy {
            max_chars: usize,
        }
        db.write(|w| w.save_policy("global", &Policy { max_chars: 10 })).unwrap();
        db.read(|r| {
            assert_eq!(r.load_policy::<Policy>("global")?, Some(Policy { max_chars: 10 }));
            assert_eq!(r.load_policy::<Policy>("workspace:1")?, None);
            Ok(())
        })
        .unwrap();

        db.write(|w| {
            w.set_setting("theme", "dark")?;
            w.set_setting("theme", "light")?;
            w.set_setting_json("recent", &vec![1, 2])
        })
        .unwrap();
        db.read(|r| {
            assert_eq!(r.setting("theme")?.as_deref(), Some("light"));
            assert_eq!(r.setting_json::<Vec<i32>>("recent")?, Some(vec![1, 2]));
            // A value that no longer decodes is treated as unset.
            assert_eq!(r.setting_json::<Vec<i32>>("theme")?, None);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn unfiltered_updates_and_deletes_are_refused() {
        let mut db = db();
        let err = db
            .write(|w| Settings::update().set(Settings::Value, "x").execute(w))
            .unwrap_err();
        assert!(err.to_string().contains("no filter"), "{err}");
        let err = db.write(|w| Settings::delete().execute(w)).unwrap_err();
        assert!(err.to_string().contains("no filter"), "{err}");
    }
}
