// @generated automatically by Diesel CLI.

diesel::table! {
    drivers (driver_id) {
        driver_id -> Uuid,
        driver_pubkey -> Jsonb,
        name -> Text,
        email -> Text,
        phone -> Text,
        status -> Text,
        driver_location -> Jsonb,
        license_number -> Nullable<Text>,
        vehicle_type -> Text,
        driver_response -> Jsonb,
        vehicle -> Nullable<Text>,
    }
}

diesel::table! {
    ride_request (request_id) {
        request_id -> Uuid,
        rider_id -> Uuid,
        pick_up -> Jsonb,
        drop_off -> Jsonb,
        estimated_price -> Int8,
        distance_km -> Float8,
        estimated_time_min -> Int4,
        ride_type -> Jsonb,
        items -> Jsonb,
        payment_method -> Text,
    }
}

diesel::table! {
    riders (rider_id) {
        rider_id -> Uuid,
        rider_pubkey -> Jsonb,
        name -> Text,
        email -> Text,
        phone -> Text,
    }
}

diesel::table! {
    trips (trip_id) {
        trip_id -> Bytea,
        rider_id -> Uuid,
        reference -> Text,
        pick_up -> Text,
        drop_off -> Text,
        driver_location -> Text,
        rider_pubkey -> Text,
        driver_pubkey -> Text,
        driver_id -> Uuid,
        status -> Text,
        start_ts -> Int8,
        end_ts -> Nullable<Int8>,
        distance_km -> Float8,
        item -> Jsonb,
        fare_estimate -> Nullable<Int8>,
        fare_lamports -> Nullable<Int8>,
        rider_email -> Text,
    }
}

diesel::allow_tables_to_appear_in_same_query!(drivers, ride_request, riders, trips,);
