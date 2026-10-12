-- When the driver's position last came in, so matching can skip drivers whose location is stale.
-- NULL until the driver's app reports a position; such drivers are never offered rides.
ALTER TABLE back_drivers ADD COLUMN location_updated_at timestamptz;
