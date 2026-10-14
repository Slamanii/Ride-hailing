-- Cities ASAP delivers in: a circle around each centre. Pickup and drop-off must both fall
-- inside the same active area. Add a city with one INSERT; switch it off with active = false.
CREATE TABLE back_service_areas (
    id          SERIAL PRIMARY KEY,
    city        TEXT NOT NULL UNIQUE,
    center_lat  DOUBLE PRECISION NOT NULL CHECK (center_lat BETWEEN -90 AND 90),
    center_lng  DOUBLE PRECISION NOT NULL CHECK (center_lng BETWEEN -180 AND 180),
    radius_km   DOUBLE PRECISION NOT NULL CHECK (radius_km > 0),
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Read and written only by the Rust backend; no policies, so the app's roles can't touch it.
ALTER TABLE back_service_areas ENABLE ROW LEVEL SECURITY;

INSERT INTO back_service_areas (city, center_lat, center_lng, radius_km)
VALUES ('Abuja', 9.0765, 7.3986, 35);
