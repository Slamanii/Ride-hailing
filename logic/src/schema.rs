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

    app_custom_users (id) {
        id -> Uuid,
        created_at -> Timestamptz,
        username -> Text,
        custom_role -> CustomRoles,
        phone -> Nullable<Text>,
        profileImage -> Nullable<Text>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::DeliveryStatus;

    app_delivery_orders (id) {
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
        status -> DeliveryStatus,
        order_code -> Text,
        driver_id -> Nullable<Uuid>,
        modified_at -> Timestamptz,
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
        initial_waypoints -> Nullable<Jsonb>,
        source -> Text,
        customer_phone -> Nullable<Text>,
        pickup_contact_name -> Nullable<Text>,
        pickup_contact_phone -> Nullable<Text>,
        payment_reference -> Nullable<Text>,
        cancelled_at -> Nullable<Timestamptz>,
        cancelled_by -> Nullable<Uuid>,
        order_ref -> Nullable<Uuid>,
        price -> Nullable<Numeric>,
        insurance -> Nullable<Numeric>,
    }
}

diesel::table! {
    app_delivery_orders_waypoints (created_at, id) {
        order_id -> Int8,
        lat -> Nullable<Numeric>,
        long -> Nullable<Numeric>,
        created_at -> Timestamptz,
        id -> Int8,
    }
}

diesel::table! {
    app_locations (id) {
        id -> Int8,
        created_at -> Timestamptz,
        longitude -> Numeric,
        latitude -> Numeric,
        frontend_order_id -> Uuid,
    }
}

diesel::table! {
    app_messages (id) {
        id -> Int8,
        created_at -> Timestamptz,
        sender_id -> Uuid,
        receiver_id -> Uuid,
        delivery_order_id -> Int8,
        message -> Text,
        is_read -> Bool,
        image_url -> Nullable<Text>,
    }
}

diesel::table! {
    app_package_images (id) {
        id -> Int8,
        created_at -> Timestamptz,
        url -> Text,
        user_id -> Uuid,
    }
}

diesel::table! {
    app_ride_offers (id) {
        id -> Uuid,
        order_id -> Int8,
        driver_id -> Uuid,
        status -> Text,
        created_at -> Timestamptz,
        expires_at -> Timestamptz,
        responded_at -> Nullable<Timestamptz>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::CustomRoles;

    app_riders_current_status (id) {
        id -> Uuid,
        updated_at -> Timestamptz,
        email -> Varchar,
        longitude -> Numeric,
        latitude -> Numeric,
        active_mode -> CustomRoles,
    }
}

diesel::table! {
    app_saved_locations (id) {
        id -> Int8,
        created_at -> Timestamptz,
        name -> Nullable<Text>,
        latitude -> Nullable<Numeric>,
        longitude -> Nullable<Numeric>,
        user_id -> Nullable<Uuid>,
    }
}

diesel::table! {
    back_custom_users (rider_id) {
        rider_id -> Uuid,
        rider_pubkey -> Jsonb,
        name -> Text,
        email -> Text,
        phone -> Text,
    }
}

diesel::table! {
    back_drivers (driver_id) {
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
        account_number -> Nullable<Text>,
        bank_code -> Nullable<Text>,
        tier -> Nullable<Text>,
        location_updated_at -> Nullable<Timestamptz>,
        plate_number -> Nullable<Text>,
    }
}

diesel::table! {
    back_ride_request (request_id) {
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
        order_ref -> Nullable<Uuid>,
        user_id -> Nullable<Int8>,
        user_phone_number -> Nullable<Text>,
        vendor_phone_number -> Nullable<Text>,
        search_attempts -> Int4,
    }
}

diesel::table! {
    back_service_areas (id) {
        id -> Int4,
        city -> Text,
        center_lat -> Float8,
        center_lng -> Float8,
        radius_km -> Float8,
        active -> Bool,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    back_trips (trip_id) {
        trip_id -> Bytea,
        rider_id -> Uuid,
        reference -> Text,
        pick_up -> Jsonb,
        drop_off -> Jsonb,
        driver_location -> Jsonb,
        rider_pubkey -> Nullable<Text>,
        driver_pubkey -> Nullable<Text>,
        driver_id -> Uuid,
        status -> Text,
        start_ts -> Int8,
        end_ts -> Nullable<Int8>,
        distance_km -> Float8,
        item -> Jsonb,
        fare_estimate -> Nullable<Int8>,
        fare_lamports -> Nullable<Int8>,
        order_ref -> Nullable<Uuid>,
        rider_email -> Text,
        asset_id -> Nullable<Uuid>,
    }
}

diesel::joinable!(app_delivery_orders_waypoints -> app_delivery_orders (order_id));
diesel::joinable!(app_messages -> app_delivery_orders (delivery_order_id));
diesel::joinable!(app_ride_offers -> app_delivery_orders (order_id));
diesel::joinable!(back_trips -> back_drivers (driver_id));

diesel::allow_tables_to_appear_in_same_query!(
    app_custom_users,
    app_delivery_orders,
    app_delivery_orders_waypoints,
    app_locations,
    app_messages,
    app_package_images,
    app_ride_offers,
    app_riders_current_status,
    app_saved_locations,
    back_custom_users,
    back_drivers,
    back_ride_request,
    back_service_areas,
    back_trips,
);
