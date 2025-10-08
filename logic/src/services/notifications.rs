use crate::{Utc, Duration};

pub fn calculate_eta(estimated_time_min) -> String {
    let eta = Utc::now() + Duration::minutes(estimated_time_min as i64);
    eta.format("%H:%M").to_string()
}