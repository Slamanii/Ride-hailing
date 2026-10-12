-- How many driver searches an order has had; each Try again widens the search radius.
ALTER TABLE back_ride_request
ADD COLUMN search_attempts INTEGER NOT NULL DEFAULT 0;
