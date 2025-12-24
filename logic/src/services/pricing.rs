use crate::api::riders::{ RideType };
use serde::{ Serialize, Deserialize };
use diesel::prelude::*;


const TRAFFIC_FACTOR: f64 = 1.3; //this needs to be recalculated using driver tracking data


pub fn distance_between(pick_up: &GeoPoint, drop_off: &GeoPoint) -> f64 {
    
    pick_up.distance_to(drop_off)
}

pub fn estimated_time_min(distance_km: f64, ride_type: &RideType) -> i32 {
    
    let traffic_factor = TRAFFIC_FACTOR;
    let avg_speed = match ride_type {
        RideType::ASAP => 30.0,         
        RideType::ASAPEXPRESS => 40.0,   
    };

    let time_hours = distance_km / (avg_speed * traffic_factor); 
    (time_hours * 60.0).round() as i32
}


pub fn calculate_asap(distance_km: f64, estimated_time_min: i32) -> i64 {
    let base_fare = 900.0;
    let per_km_rate = 25.76;
    let per_min_rate = 12.267;

    let fare = base_fare + (distance_km * per_km_rate) + (estimated_time_min as f64 * per_min_rate);
    fare.round() as i64
}


pub fn calculate_express(distance_km: f64, estimated_time_min: i32) -> i64 {
    
    let base_fare = 900.0;
    let per_km_rate = 25.76;
    let per_min_rate = 12.267;

    let fare = base_fare + (distance_km * per_km_rate) + (estimated_time_min as f64 * per_min_rate);
    fare.round() as i64
}


///part of matching service because it involves driver and ride request data

pub fn minimum_distance_between_driver_and_pickup(
    pick_up_point: GeoPoint,
    driver_current_location: GeoPoint,
) -> bool {
    let r = 6371.0; // Earth's radius in km

    let lati1 = pick_up_point.lat.to_radians();
    let long1 = pick_up_point.lng.to_radians();
    let lati2 = driver_current_location.lat.to_radians();
    let long2 = driver_current_location.lng.to_radians();

    let dlati = lati2 - lati1;
    let dlong = long2 - long1;

    // Haversine formula
    let a = (dlati / 2.0).sin().powi(2)
        + lati1.cos() * lati2.cos() * (dlong / 2.0).sin().powi(2);

    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

    let distance = r * c; // distance in kilometers

    distance < 5.0 // return true if within 5 km
}




#[derive(Serialize, Deserialize, Clone)]
pub struct GeoPoint {
    pub lat: f64,
    pub lng: f64,
    pub name: Option<String>,
}

impl std::fmt::Display for GeoPoint {

    
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}:{}:{}", self.name, self.lat, self.lng)
    }

}

impl GeoPoint {

    pub fn new(lat: f64, lng: f64, name_gp: String) -> Self {
        Self { lat, lng, name: Some(name_gp) }
    }

    pub fn distance_to(&self, other: &GeoPoint) -> f64 {
        let r = 6371.0;

        let lat1 = self.lat.to_radians();
        let lng1 = self.lng.to_radians();
        let lat2 = other.lat.to_radians();
        let lng2 = other.lng.to_radians();

        let dlat = lat2 - lat1;
        let dlng = lng2 - lng1;

        let a = (dlat / 2.0).sin().powi(2)
            + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
        let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

        r * c //distance in kilometers
    }
}

