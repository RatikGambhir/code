//! Schema migrations, applied in order and tracked with `PRAGMA user_version`.
//!
//! The database stores *application* state. It deliberately does not mirror
//! Codex's own session files or Chromium's profile: cookies, logins and Codex
//! credentials stay in the stores that own them (the browser profile directory
//! and the macOS Keychain).

/// Each entry upgrades the schema from version `index` to `index + 1`.
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
        scope              TEXT    NOT NULL,
        scope_key          TEXT    NOT NULL DEFAULT '',
        period             TEXT    NOT NULL,
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
];
