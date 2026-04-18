-- Your SQL goes here
ALTER TABLE ride_request
ADD COLUMN order_id UUID NULL,
ADD COLUMN user_id BIGINT NULL,
ADD COLUMN user_phone_number TEXT NULL,
ADD COLUMN vendor_phone_number TEXT NULL;
