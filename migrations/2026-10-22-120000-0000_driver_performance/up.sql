-- A driver's day (Lagos time), for the admin dashboard. Rebuilt from the source tables by
-- api::dashboard every 5 minutes (yesterday and today), so any period is a sum of days.
--   asset_id         the asset the day's trips ran on (else the driver's assignment that day);
--                    summing rows by asset_id gives asset performance
--   online_minutes   back_driver_sessions, clipped to the day
-- Money is kobo: completed trips' fares and the driver's and asset account's shares of them.
CREATE TABLE back_driver_performance (
    driver_id          UUID NOT NULL,
    day                DATE NOT NULL,
    asset_id           UUID,
    offers_received    INTEGER NOT NULL DEFAULT 0,
    offers_accepted    INTEGER NOT NULL DEFAULT 0,
    offers_rejected    INTEGER NOT NULL DEFAULT 0,
    offers_expired     INTEGER NOT NULL DEFAULT 0,
    trips_completed    INTEGER NOT NULL DEFAULT 0,
    trips_cancelled    INTEGER NOT NULL DEFAULT 0,
    distance_km        NUMERIC(10,2) NOT NULL DEFAULT 0,
    gross_fares        BIGINT NOT NULL DEFAULT 0,
    driver_earnings    BIGINT NOT NULL DEFAULT 0,
    asset_pool         BIGINT NOT NULL DEFAULT 0,
    disputes_raised    INTEGER NOT NULL DEFAULT 0,
    disputes_upheld    INTEGER NOT NULL DEFAULT 0,
    disputes_rejected  INTEGER NOT NULL DEFAULT 0,
    online_minutes     INTEGER NOT NULL DEFAULT 0,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (driver_id, day)
);

CREATE INDEX back_driver_performance_day ON back_driver_performance (day);
CREATE INDEX back_driver_performance_asset ON back_driver_performance (asset_id, day) WHERE asset_id IS NOT NULL;

-- No policies: only the backend reads it.
ALTER TABLE back_driver_performance ENABLE ROW LEVEL SECURITY;
