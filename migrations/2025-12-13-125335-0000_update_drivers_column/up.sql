-- Your SQL goes here
ALTER TABLE drivers
    ALTER COLUMN driver_pubkey TYPE JSONB USING driver_pubkey::JSONB;