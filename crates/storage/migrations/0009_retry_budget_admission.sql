-- Manual retries are separate budget-admission cycles for the same durable job. Keep every
-- historical reservation, allow only one active cycle at a time, and remember the append-only
-- usage-ledger boundary at which each cycle began.
CREATE TABLE budget_reservations_v2 (
    id TEXT PRIMARY KEY NOT NULL,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT,
    reconciled_at TEXT,
    usage_sequence_start INTEGER NOT NULL DEFAULT 0 CHECK (usage_sequence_start >= 0),
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

INSERT INTO budget_reservations_v2
    (id, job_id, status, created_at, expires_at, reconciled_at, usage_sequence_start, payload)
SELECT id, job_id, status, created_at, expires_at, reconciled_at, 0, payload
FROM budget_reservations;

CREATE TABLE budget_allocations_v2 (
    reservation_id TEXT NOT NULL REFERENCES budget_reservations_v2(id) ON DELETE CASCADE,
    budget_id TEXT NOT NULL REFERENCES budgets(id) ON DELETE RESTRICT,
    reserved_amount INTEGER NOT NULL CHECK (reserved_amount >= 0),
    actual_amount INTEGER CHECK (actual_amount IS NULL OR actual_amount >= 0),
    PRIMARY KEY(reservation_id, budget_id)
);

INSERT INTO budget_allocations_v2
    (reservation_id, budget_id, reserved_amount, actual_amount)
SELECT reservation_id, budget_id, reserved_amount, actual_amount
FROM budget_allocations;

DROP TABLE budget_allocations;
DROP TABLE budget_reservations;
ALTER TABLE budget_reservations_v2 RENAME TO budget_reservations;
ALTER TABLE budget_allocations_v2 RENAME TO budget_allocations;

CREATE UNIQUE INDEX budget_reservations_one_active_per_job_idx
    ON budget_reservations(job_id) WHERE status = 'active';
CREATE INDEX budget_reservations_job_history_idx
    ON budget_reservations(job_id, created_at DESC);
CREATE INDEX budget_allocations_active_idx
    ON budget_allocations(budget_id, reservation_id);
