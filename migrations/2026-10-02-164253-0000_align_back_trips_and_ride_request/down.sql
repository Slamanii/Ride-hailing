ALTER TABLE back_ride_request
ALTER COLUMN rider_id DROP NOT NULL;

ALTER TABLE back_trips
ALTER COLUMN fare_lamports SET NOT NULL;

ALTER TABLE back_trips
ALTER COLUMN driver_id DROP NOT NULL;

ALTER TABLE back_trips
DROP COLUMN rider_email;
