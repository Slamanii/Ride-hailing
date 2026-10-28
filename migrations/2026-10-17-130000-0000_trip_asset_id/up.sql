-- Needs ASAP_APPS' invest.sql applied first (invest_assets).
-- The investment asset the trip ran on, stamped at pickup from the driver's current
-- asset_assignments row. NULL: not an investment asset, so the 85/15 split applies.
-- IF NOT EXISTS: invest.sql may already have added both.
ALTER TABLE back_trips ADD COLUMN IF NOT EXISTS asset_id UUID REFERENCES invest_assets (id);
CREATE INDEX IF NOT EXISTS back_trips_asset ON back_trips (asset_id) WHERE asset_id IS NOT NULL;
