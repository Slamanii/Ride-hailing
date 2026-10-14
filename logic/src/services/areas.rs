use actix_web::{ web, Scope, HttpResponse };
use diesel::prelude::*;
use diesel::pg::PgConnection;
use serde::{ Deserialize, Serialize };
use crate::api::errors::{ ApiError, blocking };
use crate::db::DbPool;
use crate::services::pricing::{ GeoPoint, distance_between };

// Where ASAP delivers: a circle per city (back_service_areas). A delivery is allowed when its
// pickup and drop-off fall inside the same active area, for app and Zazu orders alike. The app
// asks /service-areas/check before the ride type screen; quote, ride request and checkout
// enforce the same rule, so skipping the app's check gets nowhere.

#[derive(Queryable, Selectable, Serialize, Clone, Debug)]
#[diesel(table_name = crate::schema::back_service_areas)]
pub struct ServiceArea {
    pub city: String,
    pub center_lat: f64,
    pub center_lng: f64,
    pub radius_km: f64,
}

impl ServiceArea {
    pub fn contains(&self, point: &GeoPoint) -> bool {
        let center = GeoPoint { lat: self.center_lat, lng: self.center_lng, name: None };
        distance_between(&center, point) <= self.radius_km
    }
}

pub fn active_areas(conn: &mut PgConnection) -> QueryResult<Vec<ServiceArea>> {
    use crate::schema::back_service_areas::dsl as a;

    a::back_service_areas
        .filter(a::active.eq(true))
        .order(a::city.asc())
        .select(ServiceArea::as_select())
        .load(conn)
}

// The area holding both points, or the reason there is none, worded for the customer.
pub fn area_for(areas: &[ServiceArea], pick_up: &GeoPoint, drop_off: &GeoPoint) -> Result<String, String> {
    if let Some(area) = areas.iter().find(|area| area.contains(pick_up) && area.contains(drop_off)) {
        return Ok(area.city.clone());
    }
    let cities = areas.iter().map(|area| area.city.as_str()).collect::<Vec<_>>().join(", ");
    let pickup_in = areas.iter().any(|area| area.contains(pick_up));
    let dropoff_in = areas.iter().any(|area| area.contains(drop_off));
    Err(match (cities.is_empty(), pickup_in, dropoff_in) {
        (true, _, _) => "Deliveries are paused right now".to_string(),
        (false, true, true) => format!("Pickup and drop-off must be in the same city. We deliver within: {}", cities),
        (false, true, false) => format!("We don't deliver to that drop-off yet. We deliver within: {}", cities),
        (false, false, _) => format!("We don't pick up from there yet. We deliver within: {}", cities),
    })
}

pub fn require_service_area(conn: &mut PgConnection, pick_up: &GeoPoint, drop_off: &GeoPoint) -> Result<String, ApiError> {
    area_for(&active_areas(conn)?, pick_up, drop_off).map_err(ApiError::BadRequest)
}

// The active areas, for drawing on the map.
pub async fn list_areas(pool: web::Data<DbPool>) -> HttpResponse {
    match blocking(&pool, |conn| Ok(active_areas(conn)?)).await {
        Ok(areas) => HttpResponse::Ok().json(areas),
        Err(e) => e.response(),
    }
}

#[derive(Deserialize)]
pub struct AreaCheckRequest {
    pub pick_up: GeoPoint,
    pub drop_off: GeoPoint,
}

#[derive(Serialize)]
pub struct AreaCheck {
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// The app's gate to the ride type screen. Always 200: allowed says whether to go on.
pub async fn check_area(pool: web::Data<DbPool>, body: web::Json<AreaCheckRequest>) -> HttpResponse {
    let AreaCheckRequest { pick_up, drop_off } = body.into_inner();
    match blocking(&pool, |conn| Ok(active_areas(conn)?)).await {
        Ok(areas) => HttpResponse::Ok().json(match area_for(&areas, &pick_up, &drop_off) {
            Ok(city) => AreaCheck { allowed: true, city: Some(city), reason: None },
            Err(reason) => AreaCheck { allowed: false, city: None, reason: Some(reason) },
        }),
        Err(e) => e.response(),
    }
}

pub fn routes() -> Scope {
    web::scope("/service-areas")
        .route("", web::get().to(list_areas))
        .route("/check", web::post().to(check_area))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(lat: f64, lng: f64) -> GeoPoint {
        GeoPoint { lat, lng, name: None }
    }

    fn abuja() -> ServiceArea {
        ServiceArea { city: "Abuja".into(), center_lat: 9.0765, center_lng: 7.3986, radius_km: 35.0 }
    }

    #[test]
    fn both_points_must_share_an_area() {
        let areas = vec![abuja()];
        let wuse = point(9.0700, 7.4800);
        let kubwa = point(9.1550, 7.3220);
        let kaduna = point(10.5105, 7.4165);

        assert_eq!(area_for(&areas, &wuse, &kubwa), Ok("Abuja".to_string()));
        assert!(area_for(&areas, &wuse, &kaduna).unwrap_err().contains("drop-off"));
        assert!(area_for(&areas, &kaduna, &wuse).unwrap_err().contains("pick up"));
        assert!(area_for(&[], &wuse, &kubwa).is_err());
    }

    #[test]
    fn points_in_two_different_cities_are_refused() {
        let lagos = ServiceArea { city: "Lagos".into(), center_lat: 6.5244, center_lng: 3.3792, radius_km: 40.0 };
        let areas = vec![abuja(), lagos];
        let err = area_for(&areas, &point(9.0700, 7.4800), &point(6.4500, 3.4000)).unwrap_err();
        assert!(err.contains("same city"));
    }
}
