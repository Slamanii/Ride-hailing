-- Your SQL goes here
CREATE TABLE trips (
    trip_id BYTEA PRIMARY KEY,        -- corresponds to [u8; 32]
    rider_id UUID NOT NULL,
    reference UUID NOT NULL,
    pick_up TEXT NOT NULL,
    drop_off TEXT NOT NULL,
    driver_location TEXT NOT NULL,
    rider_pubkey TEXT NOT NULL,       -- Pubkey stored as string
    driver_pubkey TEXT NOT NULL,      -- Pubkey stored as string
    driver_id UUID NOT NULL,
    status TEXT NOT NULL,
    start_ts BIGINT NOT NULL,
    end_ts BIGINT,
    distance_km DOUBLE PRECISION NOT NULL,
    item JSONB NOT NULL,              -- Vec<ItemDetails> stored as JSON
    fare_estimate BIGINT,
    fare_lamports BIGINT
);
