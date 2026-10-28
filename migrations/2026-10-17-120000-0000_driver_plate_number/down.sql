UPDATE back_drivers
   SET license_number = plate_number
 WHERE license_number IS NULL AND plate_number ~ '^ASP-';

ALTER TABLE back_drivers DROP COLUMN plate_number;
