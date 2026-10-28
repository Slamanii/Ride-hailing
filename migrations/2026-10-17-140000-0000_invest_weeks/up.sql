-- Weeks whose investor stats and earnings are written (services::invest). A week is closed
-- once; the row is inserted in the same transaction as its stats and earnings, so a week is
-- never paid twice. Needs ASAP_APPS' invest.sql only at runtime, not here.
CREATE TABLE back_invest_weeks (
    week_start DATE PRIMARY KEY CHECK (extract(isodow FROM week_start) = 1),
    closed_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE back_invest_weeks ENABLE ROW LEVEL SECURITY;
