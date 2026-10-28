-- Lagos: 40km around the mainland centre (Yaba), which reaches Ikeja, Ikorodu, the Island,
-- Lekki and Ajah; Epe and Badagry fall outside.
INSERT INTO back_service_areas (city, center_lat, center_lng, radius_km)
VALUES ('Lagos', 6.5244, 3.3792, 40)
ON CONFLICT (city) DO UPDATE SET center_lat = EXCLUDED.center_lat, center_lng = EXCLUDED.center_lng,
                                 radius_km = EXCLUDED.radius_km, active = true;
