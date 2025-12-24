-- Your SQL goes here
CREATE TABLE ride_request (
    request_id UUID PRIMARY KEY,
    rider_id UUID NOT NULL,
    pick_up JSONB NOT NULL,
    drop_off JSONB NOT NULL,
    estimated_price BIGINT NOT NULL,
    distance_km DOUBLE PRECISION NOT NULL,
    estimated_time_min INT NOT NULL,
    ride_type JSONB NOT NULL,
    items JSONB NOT NULL,           -- Vec<ItemDetails> as JSON
    payment_method TEXT NOT NULL
);
