-- Fails if any trip was stored without pubkeys.
ALTER TABLE back_trips
ALTER COLUMN rider_pubkey SET NOT NULL;

ALTER TABLE back_trips
ALTER COLUMN driver_pubkey SET NOT NULL;
