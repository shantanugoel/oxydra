CREATE TABLE IF NOT EXISTS schedule_runs (
    run_id         TEXT PRIMARY KEY,
    schedule_id    TEXT NOT NULL,
    started_at     TEXT NOT NULL,
    finished_at    TEXT NOT NULL,
    status         TEXT NOT NULL,
    output_summary TEXT,
    turn_count     INTEGER NOT NULL DEFAULT 0,
    cost           REAL NOT NULL DEFAULT 0.0,
    notified       INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (schedule_id) REFERENCES schedules(schedule_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_schedule_runs_lookup
    ON schedule_runs(schedule_id, started_at DESC);
