ALTER TABLE trips
    ALTER COLUMN reference TYPE UUID USING reference::UUID;
