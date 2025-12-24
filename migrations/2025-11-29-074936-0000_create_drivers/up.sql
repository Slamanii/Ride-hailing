-- Your SQL goes here
CREATE TABLE drivers (
    driver_id UUID PRIMARY KEY,
    driver_pubkey JSONB NOT NULL,
    name TEXT NOT NULL,
    email TEXT UNIQUE NOT NULL,
    phone TEXT NOT NULL,
    status TEXT NOT NULL,
    driver_location JSONB NOT NULL,
    license_number TEXT,
    vehicle_type TEXT NOT NULL,
    driver_response JSONB NOT NULL,
    vehicle TEXT
);
