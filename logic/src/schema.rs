// @generated automatically by Diesel CLI.

pub mod sql_types {
    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "custom_roles"))]
    pub struct CustomRoles;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "delivery_status"))]
    pub struct DeliveryStatus;
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::CustomRoles;

    custom_users (id) {
        id -> Uuid,
        created_at -> Timestamptz,
        username -> Text,
        custom_role -> CustomRoles,
        phone -> Nullable<Int8>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::DeliveryStatus;

    delivery_orders (id) {
        id -> Int8,
        created_at -> Timestamptz,
        client_id -> Uuid,
        image_url -> Nullable<Text>,
        pickup_lat -> Nullable<Numeric>,
        pickup_long -> Nullable<Numeric>,
        dropoff_lat -> Nullable<Numeric>,
        dropoff_long -> Nullable<Numeric>,
        driver_initial_lat -> Nullable<Numeric>,
        driver_initial_long -> Nullable<Numeric>,
        driver_package_current_lat -> Nullable<Numeric>,
        driver_package_current_long -> Nullable<Numeric>,
        status -> DeliveryStatus,
        order_code -> Text,
        driver_id -> Nullable<Uuid>,
        modified_at -> Timestamptz,
        drivers_waypoints -> Nullable<Jsonb>,
        pickup_name -> Nullable<Text>,
        dropoff_name -> Nullable<Text>,
        dropoff_code -> Nullable<Text>,
        pickup_code -> Nullable<Text>,
        is_dropoff_code_authenticated -> Bool,
        is_pickup_code_authenticated -> Bool,
        pickup_time -> Nullable<Timestamptz>,
        dropoff_time -> Nullable<Timestamptz>,
        delivery_accepted_time -> Nullable<Timestamptz>,
        package_description -> Nullable<Text>,
        package_type -> Nullable<Text>,
    }
}

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
    locations (id) {
        id -> Int8,
        created_at -> Timestamptz,
        longitude -> Numeric,
        latitude -> Numeric,
        frontend_order_id -> Uuid,
    }
}

diesel::table! {
    messages (id) {
        id -> Int8,
        created_at -> Timestamptz,
        sender_id -> Uuid,
        receiver_id -> Uuid,
        delivery_order_id -> Int8,
        message -> Text,
        is_read -> Bool,
    }
}

diesel::table! {
    package_images (id) {
        id -> Int8,
        created_at -> Timestamptz,
        url -> Text,
        user_id -> Uuid,
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
        order_id -> Nullable<Uuid>,
        user_id -> Nullable<Int8>,
        user_phone_number -> Nullable<Text>,
        vendor_phone_number -> Nullable<Text>,
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
    use diesel::sql_types::*;
    use super::sql_types::CustomRoles;

    riders_current_status (id) {
        id -> Uuid,
        updated_at -> Timestamptz,
        email -> Varchar,
        longitude -> Numeric,
        latitude -> Numeric,
        active_mode -> CustomRoles,
    }
}

diesel::table! {
    saved_locations (id) {
        id -> Int8,
        created_at -> Timestamptz,
        name -> Nullable<Text>,
        latitude -> Nullable<Numeric>,
        longitude -> Nullable<Numeric>,
        user_id -> Nullable<Uuid>,
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

diesel::joinable!(messages -> delivery_orders (delivery_order_id));

diesel::allow_tables_to_appear_in_same_query!(
    custom_users,
    delivery_orders,
    drivers,
    locations,
    messages,
    package_images,
    ride_request,
    riders,
    riders_current_status,
    saved_locations,
    trips,
);
