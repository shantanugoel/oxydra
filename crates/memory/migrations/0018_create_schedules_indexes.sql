CREATE INDEX IF NOT EXISTS idx_schedules_user_id ON schedules(user_id);
CREATE INDEX IF NOT EXISTS idx_schedules_due ON schedules(next_run_at)
    WHERE status = 'active' AND next_run_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_schedules_status ON schedules(user_id, status);
CREATE INDEX IF NOT EXISTS idx_schedules_name ON schedules(user_id, name);
