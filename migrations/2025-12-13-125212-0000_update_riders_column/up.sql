ALTER TABLE riders
    ALTER COLUMN rider_pubkey TYPE JSONB USING rider_pubkey::JSONB;