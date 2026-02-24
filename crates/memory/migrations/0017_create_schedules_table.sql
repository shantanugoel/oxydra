CREATE TABLE IF NOT EXISTS schedules (
    schedule_id          TEXT PRIMARY KEY,
    user_id              TEXT NOT NULL,
    name                 TEXT,
    goal                 TEXT NOT NULL,
    cadence_json         TEXT NOT NULL,
    notification_policy  TEXT NOT NULL DEFAULT 'always',
    status               TEXT NOT NULL DEFAULT 'active',
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL,
    next_run_at          TEXT,
    last_run_at          TEXT,
    last_run_status      TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0
);
