-- Paystack needs the rider's email on every trip (table is empty at time of writing).
ALTER TABLE back_trips
ADD COLUMN rider_email TEXT NOT NULL;

-- Every trip must have a driver.
ALTER TABLE back_trips
ALTER COLUMN driver_id SET NOT NULL;

-- Fare in lamports is only known once the on-chain payment settles.
ALTER TABLE back_trips
ALTER COLUMN fare_lamports DROP NOT NULL;

-- Every ride request has a rider, Zazu requests included.
ALTER TABLE back_ride_request
ALTER COLUMN rider_id SET NOT NULL;
