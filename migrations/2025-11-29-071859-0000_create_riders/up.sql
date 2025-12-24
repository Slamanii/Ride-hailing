-- Your SQL goes here
CREATE TABLE riders (
    rider_id UUID PRIMARY KEY,
    rider_pubkey JSONB NOT NULL,
    name TEXT NOT NULL,
    email TEXT UNIQUE NOT NULL,
    phone TEXT NOT NULL
);
