-- This file should undo anything in `up.sql`
ALTER TABLE ride_request
DROP COLUMN vendor_phone_number,
DROP COLUMN user_phone_number,
DROP COLUMN user_id,
DROP COLUMN order_id;
