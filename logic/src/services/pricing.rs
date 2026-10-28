use crate::api::riders::{ ItemDetails, RideType };
use serde::{ Serialize, Deserialize };


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


// ---------- Package insurance ----------
// Added to the distance/time fare, naira. Each item is priced by its type on its declared
// value (item.price, already the whole line, not per unit), matched by name as the app's
// ITEM_TYPES spell them (case-insensitive):
//   everyday types      free up to ₦300,000 declared; above that, 1% of the whole value
//   risky types         rate% of the declared value, at least min_premium
//   Hazardous / Restricted Items: not delivered (ItemDetails::validate refuses them)
// An item with no type yet (the route-only quote) adds nothing; a type the list doesn't know is
// priced as "Other".

#[derive(Debug, PartialEq)]
enum Cover {
    // Free up to threshold; past it, rate% of the whole value.
    AboveThreshold { threshold: f64, rate: f64 },
    // rate% of the value, never less than min_premium.
    Always { rate: f64, min_premium: f64 },
    Blocked,
}

fn cover(package_type: &str) -> Cover {
    match package_type.trim().to_ascii_lowercase().as_str() {
        "documents" | "clothing & fashion items" | "medical supplies" | "food & groceries" | "gifts & flowers" =>
            Cover::AboveThreshold { threshold: 300_000.0, rate: 1.0 },
        "industrial parts" => Cover::Always { rate: 1.0, min_premium: 200.0 },
        "fragile items" => Cover::Always { rate: 1.5, min_premium: 300.0 },
        "electronics" => Cover::Always { rate: 1.5, min_premium: 300.0 },
        "valuables" => Cover::Always { rate: 2.0, min_premium: 500.0 },
        "oversized / heavy items" => Cover::Always { rate: 1.0, min_premium: 200.0 },
        "hazardous / restricted items" => Cover::Blocked,
        _ => Cover::Always { rate: 1.0, min_premium: 150.0 },
    }
}

pub fn is_blocked_type(package_type: &str) -> bool {
    cover(package_type) == Cover::Blocked
}

fn item_premium(item: &ItemDetails) -> f64 {
    let Some(package_type) = item.package_type.as_deref() else { return 0.0 };
    match cover(package_type) {
        Cover::AboveThreshold { threshold, rate } if item.price > threshold => item.price * rate / 100.0,
        Cover::AboveThreshold { .. } => 0.0,
        Cover::Always { rate, min_premium } => (item.price * rate / 100.0).max(min_premium),
        Cover::Blocked => 0.0,
    }
}

// The whole package's insurance, naira, rounded up so cover is never undercharged.
pub fn insurance_premium(items: &[ItemDetails]) -> i64 {
    items.iter().map(item_premium).sum::<f64>().ceil() as i64
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
    // The customer app sends { latitude, longitude }.
    #[serde(alias = "latitude")]
    pub lat: f64,
    #[serde(alias = "longitude")]
    pub lng: f64,
    #[serde(default)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(package_type: Option<&str>, price: f64) -> ItemDetails {
        ItemDetails {
            name: "Box".into(), price, dimensions: (10.0, 10.0, 10.0), quantity: 3, weight: 1.0,
            image_url: None, package_type: package_type.map(Into::into),
        }
    }

    #[test]
    fn everyday_types_are_free_up_to_the_threshold_then_charge_the_whole_value() {
        assert_eq!(insurance_premium(&[item(Some("Documents"), 300_000.0)]), 0);
        // Past ₦300,000: 1% of all of it, not of the ₦100,000 above.
        assert_eq!(insurance_premium(&[item(Some("Clothing & Fashion Items"), 400_000.0)]), 4_000);
    }

    #[test]
    fn risky_types_charge_their_rate_with_a_minimum() {
        assert_eq!(insurance_premium(&[item(Some("Electronics"), 100_000.0)]), 1_500);
        assert_eq!(insurance_premium(&[item(Some("Valuables"), 10_000.0)]), 500); // 2% = ₦200 < min
        // Price is the whole line: quantity 3 doesn't multiply it.
        assert_eq!(insurance_premium(&[item(Some("industrial parts"), 50_000.0)]), 500);
        // Unknown types price as Other.
        assert_eq!(insurance_premium(&[item(Some("Mystery"), 1_000.0)]), 150);
    }

    #[test]
    fn untyped_items_add_nothing_and_items_add_up() {
        assert_eq!(insurance_premium(&[item(None, 1_000_000.0)]), 0);
        assert_eq!(
            insurance_premium(&[item(Some("Fragile Items"), 10_000.0), item(Some("Electronics"), 200_000.0)]),
            300 + 3_000
        );
    }

    #[test]
    fn hazardous_items_are_blocked() {
        assert!(is_blocked_type("Hazardous / Restricted Items"));
        assert!(!is_blocked_type("Other"));
    }
}

