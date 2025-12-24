-- This file should undo anything in `up.sql`
 ALTER TABLE  drivers
    ALTER COLUMN driver_pubkey TYPE TEXT USING driver_pubkey::TEXT;