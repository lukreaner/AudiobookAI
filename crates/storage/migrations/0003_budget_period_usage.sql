CREATE TABLE budget_period_usage (
    budget_id TEXT NOT NULL REFERENCES budgets(id) ON DELETE CASCADE,
    period_started_at TEXT NOT NULL,
    period_ends_at TEXT NOT NULL,
    used_value INTEGER NOT NULL CHECK (used_value >= 0),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (budget_id, period_started_at)
);

CREATE INDEX budget_period_usage_window_idx
    ON budget_period_usage(budget_id, period_ends_at);
