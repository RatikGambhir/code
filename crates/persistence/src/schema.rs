//! The database schema: DDL migrations, plus one column enum per table for
//! the query builder.
//!
//! The database stores *application* state. It deliberately does not mirror
//! Codex's own session files or Chromium's profile: cookies, logins and Codex
//! credentials stay in the stores that own them (the browser profile directory
//! and the macOS Keychain).
//!
//! When a migration adds or renames a column, update the matching enum below;
//! `every_table_enum_matches_the_schema` fails until they agree.

use crate::{sql_enum, table};

/// Each entry upgrades the schema from version `index` to `index + 1`, inside
/// its own transaction.
pub(crate) const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
    CREATE TABLE workspaces (
        id            INTEGER PRIMARY KEY,
        path          TEXT    NOT NULL UNIQUE,
        name          TEXT    NOT NULL,
        last_opened   INTEGER NOT NULL,
        layout_json   TEXT
    );

    CREATE TABLE tabs (
        id            INTEGER PRIMARY KEY,
        workspace_id  INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        pane          INTEGER NOT NULL DEFAULT 0,
        kind          TEXT    NOT NULL CHECK (kind IN ('file', 'browser', 'diff')),
        resource      TEXT    NOT NULL,
        position      INTEGER NOT NULL,
        active        INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX tabs_workspace ON tabs(workspace_id, pane, position);

    CREATE TABLE browser_profiles (
        id            INTEGER PRIMARY KEY,
        name          TEXT    NOT NULL UNIQUE,
        storage_dir   TEXT    NOT NULL,
        created_at    INTEGER NOT NULL
    );

    CREATE TABLE browser_history (
        id            INTEGER PRIMARY KEY,
        profile_id    INTEGER REFERENCES browser_profiles(id) ON DELETE CASCADE,
        url           TEXT    NOT NULL,
        title         TEXT,
        visited_at    INTEGER NOT NULL
    );
    CREATE INDEX browser_history_visited ON browser_history(visited_at DESC);
    CREATE INDEX browser_history_url ON browser_history(url);

    CREATE TABLE agent_threads (
        thread_id     TEXT    PRIMARY KEY,
        workspace_id  INTEGER REFERENCES workspaces(id) ON DELETE SET NULL,
        title         TEXT,
        model         TEXT,
        cwd           TEXT,
        created_at    INTEGER NOT NULL,
        updated_at    INTEGER NOT NULL,
        archived      INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX agent_threads_workspace ON agent_threads(workspace_id, updated_at DESC);

    -- Append-only log of what agents did, for diagnostics and analytics.
    CREATE TABLE agent_events (
        id            INTEGER PRIMARY KEY,
        thread_id     TEXT    NOT NULL,
        turn_id       TEXT,
        event_type    TEXT    NOT NULL,
        payload       TEXT    NOT NULL,
        created_at    INTEGER NOT NULL
    );
    CREATE INDEX agent_events_thread ON agent_events(thread_id, id);

    CREATE TABLE token_usage (
        id                       INTEGER PRIMARY KEY,
        thread_id                TEXT,
        turn_id                  TEXT,
        workspace_id             INTEGER,
        model                    TEXT    NOT NULL,
        input_tokens             INTEGER NOT NULL,
        cached_input_tokens      INTEGER NOT NULL,
        output_tokens            INTEGER NOT NULL,
        reasoning_output_tokens  INTEGER NOT NULL,
        total_tokens             INTEGER NOT NULL,
        recorded_at              INTEGER NOT NULL
    );
    CREATE INDEX token_usage_recorded ON token_usage(recorded_at);
    CREATE INDEX token_usage_workspace ON token_usage(workspace_id, recorded_at);

    CREATE TABLE budgets (
        id                 INTEGER PRIMARY KEY,
        scope              TEXT    NOT NULL CHECK (scope IN ('global', 'workspace', 'thread', 'model')),
        scope_key          TEXT    NOT NULL DEFAULT '',
        period             TEXT    NOT NULL CHECK (period IN ('day', 'week', 'month', 'total')),
        soft_limit_tokens  INTEGER,
        hard_limit_tokens  INTEGER,
        UNIQUE (scope, scope_key, period)
    );

    CREATE TABLE agent_policies (
        scope_key     TEXT    PRIMARY KEY,
        policy_json   TEXT    NOT NULL,
        updated_at    INTEGER NOT NULL
    );

    CREATE TABLE settings (
        key           TEXT    PRIMARY KEY,
        value         TEXT    NOT NULL
    );
    "#,
    // 2: the last cumulative token total Codex reported for each thread, so
    // usage can be recorded as deltas without double counting after a resume.
    r#"
    ALTER TABLE agent_threads ADD COLUMN usage_baseline TEXT;
    "#,
    // 3: which agent provider a thread belongs to ("openai", "anthropic").
    // Older rows are Codex threads.
    r#"
    ALTER TABLE agent_threads ADD COLUMN provider TEXT;
    "#,
];

table! {
    /// Projects the user has opened.
    pub enum Workspaces = "workspaces" {
        Id = "id",
        Path = "path",
        Name = "name",
        LastOpened = "last_opened",
        LayoutJson = "layout_json",
    }
}

table! {
    /// Editor and browser tabs to restore, per workspace and pane.
    pub enum Tabs = "tabs" {
        Id = "id",
        WorkspaceId = "workspace_id",
        Pane = "pane",
        Kind = "kind",
        Resource = "resource",
        Position = "position",
        Active = "active",
    }
}

table! {
    /// Named browser profiles. Chromium keeps the actual cookies and storage
    /// in `storage_dir`.
    pub enum BrowserProfiles = "browser_profiles" {
        Id = "id",
        Name = "name",
        StorageDir = "storage_dir",
        CreatedAt = "created_at",
    }
}

table! {
    pub enum BrowserHistory = "browser_history" {
        Id = "id",
        ProfileId = "profile_id",
        Url = "url",
        Title = "title",
        VisitedAt = "visited_at",
    }
}

table! {
    /// Agent threads (Codex or Claude) the IDE knows about, so they can be
    /// resumed.
    pub enum AgentThreads = "agent_threads" {
        ThreadId = "thread_id",
        WorkspaceId = "workspace_id",
        Title = "title",
        Model = "model",
        Cwd = "cwd",
        CreatedAt = "created_at",
        UpdatedAt = "updated_at",
        Archived = "archived",
        /// JSON `TokenCounts`: the thread's cumulative usage last recorded.
        UsageBaseline = "usage_baseline",
        /// "openai" or "anthropic"; NULL for threads from before providers.
        Provider = "provider",
    }
}

table! {
    pub enum AgentEvents = "agent_events" {
        Id = "id",
        ThreadId = "thread_id",
        TurnId = "turn_id",
        EventType = "event_type",
        Payload = "payload",
        CreatedAt = "created_at",
    }
}

table! {
    pub enum TokenUsage = "token_usage" {
        Id = "id",
        ThreadId = "thread_id",
        TurnId = "turn_id",
        WorkspaceId = "workspace_id",
        Model = "model",
        InputTokens = "input_tokens",
        CachedInputTokens = "cached_input_tokens",
        OutputTokens = "output_tokens",
        ReasoningOutputTokens = "reasoning_output_tokens",
        TotalTokens = "total_tokens",
        RecordedAt = "recorded_at",
    }
}

table! {
    pub enum Budgets = "budgets" {
        Id = "id",
        Scope = "scope",
        ScopeKey = "scope_key",
        Period = "period",
        SoftLimitTokens = "soft_limit_tokens",
        HardLimitTokens = "hard_limit_tokens",
    }
}

table! {
    pub enum AgentPolicies = "agent_policies" {
        ScopeKey = "scope_key",
        PolicyJson = "policy_json",
        UpdatedAt = "updated_at",
    }
}

table! {
    pub enum Settings = "settings" {
        Key = "key",
        Value = "value",
    }
}

sql_enum! {
    /// What a restored tab shows.
    pub enum TabKind {
        File = "file",
        Browser = "browser",
        Diff = "diff",
    }
}

sql_enum! {
    /// What a token budget is counted over.
    pub enum BudgetScope {
        /// Everything the app spends.
        Global = "global",
        /// One workspace; the scope key is its id.
        Workspace = "workspace",
        /// One agent thread; the scope key is the thread id.
        Thread = "thread",
        /// One model; the scope key is the model id.
        Model = "model",
    }
}

sql_enum! {
    /// The window a budget resets over.
    pub enum BudgetPeriod {
        Day = "day",
        Week = "week",
        Month = "month",
        /// Never resets.
        Total = "total",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::Column;
    use std::collections::BTreeSet;

    fn schema_columns(conn: &rusqlite::Connection, table: &str) -> BTreeSet<String> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn enum_columns<C: Column>() -> (&'static str, BTreeSet<String>) {
        (C::TABLE, C::ALL.iter().map(|c| c.name().to_string()).collect())
    }

    #[test]
    fn every_table_enum_matches_the_schema() {
        let db = crate::Database::open_in_memory().unwrap();
        let tables = [
            enum_columns::<Workspaces>(),
            enum_columns::<Tabs>(),
            enum_columns::<BrowserProfiles>(),
            enum_columns::<BrowserHistory>(),
            enum_columns::<AgentThreads>(),
            enum_columns::<AgentEvents>(),
            enum_columns::<TokenUsage>(),
            enum_columns::<Budgets>(),
            enum_columns::<AgentPolicies>(),
            enum_columns::<Settings>(),
        ];
        for (table, columns) in tables {
            assert_eq!(schema_columns(db.connection(), table), columns, "table {table}");
        }
    }

    #[test]
    fn check_constraints_match_the_value_enums() {
        // The CHECK lists in the DDL must accept every enum value.
        let ddl = MIGRATIONS.concat();
        for kind in TabKind::ALL {
            assert!(ddl.contains(&format!("'{kind}'")), "{kind}");
        }
        for scope in BudgetScope::ALL {
            assert!(ddl.contains(&format!("'{scope}'")), "{scope}");
        }
        for period in BudgetPeriod::ALL {
            assert!(ddl.contains(&format!("'{period}'")), "{period}");
        }
    }
}
