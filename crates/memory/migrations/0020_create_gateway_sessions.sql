CREATE TABLE IF NOT EXISTS gateway_sessions (
    session_id        TEXT PRIMARY KEY,
    user_id           TEXT NOT NULL,
    agent_name        TEXT NOT NULL DEFAULT 'default',
    display_name      TEXT,
    channel_origin    TEXT NOT NULL DEFAULT 'tui',
    parent_session_id TEXT,
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    last_active_at    TEXT NOT NULL DEFAULT (datetime('now')),
    archived          INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (parent_session_id) REFERENCES gateway_sessions(session_id)
        ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_gateway_sessions_user_active
    ON gateway_sessions(user_id, archived, last_active_at DESC);

CREATE INDEX IF NOT EXISTS idx_gateway_sessions_parent
    ON gateway_sessions(parent_session_id)
    WHERE parent_session_id IS NOT NULL;
