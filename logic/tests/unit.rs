use logic::services::pricing::{
    GeoPoint, distance_between, minimum_distance_between_driver_and_pickup,
    calculate_asap, calculate_express, estimated_time_min,
};
use logic::api::riders::{RideType, ItemDetails};
use logic::api::trips::Trip;
use logic::services::escrow::{vec_to_array_32, i64_to_u64};
use uuid::Uuid;


// ─── GeoPoint ────────────────────────────────────────────────────────────────

#[test]
fn geopoint_distance_to_self_is_zero() {
    let p = GeoPoint { lat: 6.5244, lng: 3.3792, name: None };
    assert_eq!(p.distance_to(&p), 0.0);
}

#[test]
fn geopoint_distance_known_coords() {
    // Lagos Island → Victoria Island, roughly 3–4 km
    let pickup  = GeoPoint { lat: 6.4531, lng: 3.3958, name: None };
    let dropoff = GeoPoint { lat: 6.4280, lng: 3.4219, name: None };
    let dist = distance_between(&pickup, &dropoff);
    assert!(dist > 2.0 && dist < 5.0, "expected ~3 km, got {:.3}", dist);
}

#[test]
fn geopoint_distance_is_symmetric() {
    let a = GeoPoint { lat: 6.5244, lng: 3.3792, name: None };
    let b = GeoPoint { lat: 6.4531, lng: 3.3958, name: None };
    let diff = (a.distance_to(&b) - b.distance_to(&a)).abs();
    assert!(diff < 1e-9, "distance should be symmetric, diff was {}", diff);
}


// ─── minimum_distance_between_driver_and_pickup ───────────────────────────────

#[test]
fn driver_within_5km_of_pickup_returns_true() {
    let pickup = GeoPoint { lat: 6.5244, lng: 3.3792, name: None };
    let driver = GeoPoint { lat: 6.5300, lng: 3.3850, name: None }; // ~0.8 km away
    assert!(minimum_distance_between_driver_and_pickup(pickup, driver));
}

#[test]
fn driver_beyond_5km_of_pickup_returns_false() {
    let pickup = GeoPoint { lat: 6.5244, lng: 3.3792, name: None };
    let driver = GeoPoint { lat: 6.9000, lng: 3.8500, name: None }; // ~60 km away
    assert!(!minimum_distance_between_driver_and_pickup(pickup, driver));
}

#[test]
fn driver_exactly_at_pickup_returns_true() {
    let pickup = GeoPoint { lat: 6.5244, lng: 3.3792, name: None };
    let driver = pickup.clone();
    assert!(minimum_distance_between_driver_and_pickup(pickup, driver));
}


// ─── Pricing ─────────────────────────────────────────────────────────────────

#[test]
fn calculate_asap_base_fare_only() {
    // 0 km, 0 min → base fare only = 900
    assert_eq!(calculate_asap(0.0, 0), 900);
}

#[test]
fn calculate_asap_with_distance_and_time() {
    // 900 + (10 * 25.76) + (20 * 12.267) = 900 + 257.6 + 245.34 = 1402.94 → 1403
    assert_eq!(calculate_asap(10.0, 20), 1403);
}

#[test]
fn calculate_express_base_fare_only() {
    assert_eq!(calculate_express(0.0, 0), 900);
}

#[test]
fn calculate_express_matches_asap_at_same_inputs() {
    // Both use identical rates currently — this test documents that intentionally
    assert_eq!(calculate_asap(5.0, 10), calculate_express(5.0, 10));
}

#[test]
fn estimated_time_asap_30km() {
    // 30 / (30 * 1.3) = 0.7692 hrs = 46.15 min → 46
    assert_eq!(estimated_time_min(30.0, &RideType::ASAP), 46);
}

#[test]
fn estimated_time_express_30km() {
    // 30 / (40 * 1.3) = 0.5769 hrs = 34.6 min → 35
    assert_eq!(estimated_time_min(30.0, &RideType::ASAPEXPRESS), 35);
}

#[test]
fn express_is_faster_than_asap_for_same_distance() {
    let asap_time    = estimated_time_min(20.0, &RideType::ASAP);
    let express_time = estimated_time_min(20.0, &RideType::ASAPEXPRESS);
    assert!(express_time < asap_time, "express should be faster than asap");
}


// ─── Trip ────────────────────────────────────────────────────────────────────

fn make_trip() -> Trip {
    Trip {
        trip_id: vec![0u8; 32],
        rider_id: Uuid::new_v4(),
        reference: "ref-test-001".to_string(),
        pick_up: "Lagos Island".to_string(),
        drop_off: "Victoria Island".to_string(),
        driver_location: "Lekki Phase 1".to_string(),
        rider_pubkey: "rider_pubkey_string".to_string(),
        driver_pubkey: "driver_pubkey_string".to_string(),
        driver_id: Uuid::new_v4(),
        status: "Ongoing".to_string(),
        start_ts: 1700000000,
        end_ts: None,
        distance_km: 10.0,
        item: serde_json::json!({}),
        fare_estimate: Some(1500),
        fare_lamports: None,
        rider_email: "rider@test.com".to_string(),
    }
}

#[test]
fn trip_status_unchanged_when_driver_not_at_dropoff() {
    let mut trip = make_trip();
    trip.update_status();
    assert_eq!(trip.status, "Ongoing");
    assert!(trip.end_ts.is_none());
}

#[test]
fn trip_status_completes_when_driver_at_dropoff() {
    let mut trip = make_trip();
    trip.driver_location = trip.drop_off.clone();
    trip.update_status();
    assert_eq!(trip.status, "Completed");
    assert!(trip.end_ts.is_some());
}

#[test]
fn trip_compute_fare_lamports_from_estimate() {
    let mut trip = make_trip(); // fare_estimate = 1500
    trip.compute_fare_lamports();
    // 1500 * 128.0 = 192_000
    assert_eq!(trip.fare_lamports, Some(192_000));
}

#[test]
fn trip_compute_fare_lamports_none_when_no_estimate() {
    let mut trip = make_trip();
    trip.fare_estimate = None;
    trip.compute_fare_lamports();
    assert_eq!(trip.fare_lamports, None);
}

#[test]
fn trip_end_ts_set_on_completion() {
    let mut trip = make_trip();
    trip.driver_location = trip.drop_off.clone();
    trip.update_status();
    let ts = trip.end_ts.unwrap();
    assert!(ts > 0, "end_ts should be a valid unix timestamp");
}


// ─── ItemDetails ─────────────────────────────────────────────────────────────

fn make_item(length: f64, width: f64, height: f64, weight: f64, quantity: u32) -> ItemDetails {
    ItemDetails {
        name: "test_item".to_string(),
        price: 500,
        dimensions: (length, width, height),
        quantity,
        weight,
    }
}

#[test]
fn item_dimensions_within_limit_ok() {
    let item = make_item(10.0, 10.0, 10.0, 1.0, 1);
    assert!(item.max_dimensions().is_ok());
}

#[test]
fn item_dimensions_at_exact_limit_ok() {
    let item = make_item(13.0, 13.0, 13.0, 1.0, 1);
    assert!(item.max_dimensions().is_ok());
}

#[test]
fn item_dimensions_over_limit_err() {
    let item = make_item(14.0, 14.0, 14.0, 1.0, 1);
    assert!(item.max_dimensions().is_err());
}

#[test]
fn item_weight_within_limit_ok() {
    let item = make_item(5.0, 5.0, 5.0, 1.0, 4); // 4 kg total
    assert!(item.max_weight().is_ok());
}

#[test]
fn item_weight_over_limit_err() {
    let item = make_item(5.0, 5.0, 5.0, 2.0, 3); // 6 kg total
    assert!(item.max_weight().is_err());
}

#[test]
fn item_weight_exactly_at_limit_ok() {
    let item = make_item(5.0, 5.0, 5.0, 2.5, 2); // exactly 5.0 kg
    assert!(item.max_weight().is_ok());
}

#[test]
fn item_quantity_within_limit_ok() {
    let item = make_item(5.0, 5.0, 5.0, 0.4, 10);
    assert!(item.aggregate_quantity().is_ok());
}

#[test]
fn item_quantity_over_limit_err() {
    let item = make_item(5.0, 5.0, 5.0, 0.4, 11);
    assert!(item.aggregate_quantity().is_err());
}


// ─── Escrow helpers ──────────────────────────────────────────────────────────

#[test]
fn i64_to_u64_positive_value_ok() {
    assert_eq!(i64_to_u64(1_000_000).unwrap(), 1_000_000u64);
}

#[test]
fn i64_to_u64_zero_ok() {
    assert_eq!(i64_to_u64(0).unwrap(), 0u64);
}

#[test]
fn i64_to_u64_negative_value_err() {
    assert!(i64_to_u64(-1).is_err());
}

#[test]
fn vec_to_array_32_exact_length_ok() {
    let v = vec![1u8; 32];
    let arr = vec_to_array_32(v.clone()).unwrap();
    assert_eq!(arr, [1u8; 32]);
}

#[test]
fn vec_to_array_32_wrong_length_err() {
    let v = vec![0u8; 16];
    assert!(vec_to_array_32(v).is_err());
}

#[test]
fn vec_to_array_32_empty_err() {
    assert!(vec_to_array_32(vec![]).is_err());
}
