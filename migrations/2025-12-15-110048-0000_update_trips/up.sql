ALTER TABLE trips
   ALTER COLUMN reference TYPE TEXT USING reference::TEXT;
