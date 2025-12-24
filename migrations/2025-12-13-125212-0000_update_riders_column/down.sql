  ALTER TABLE riders
    ALTER COLUMN rider_pubkey TYPE TEXT USING rider_pubkey::TEXT;