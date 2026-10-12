-- Riders and drivers don't have real wallets yet, so a trip may have no pubkeys. The on-chain
-- record (optional, ONCHAIN_RECORDING) needs both; everything else ignores them.
ALTER TABLE back_trips
ALTER COLUMN rider_pubkey DROP NOT NULL;

ALTER TABLE back_trips
ALTER COLUMN driver_pubkey DROP NOT NULL;
