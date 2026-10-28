use actix_web::{ web, HttpResponse };
use serde::{ Deserialize, Serialize };
use diesel::prelude::*;
use diesel::sql_types::{ Text, Uuid as SqlUuid };
use crate::api::errors::{ ApiError, blocking };
use crate::db::DbPool;
use crate::services::auth::AuthUser;
use crate::services::phone::normalize_phone;

// The customer's own name and phone, from the app's Personal info screen. Both copies change
// together: app_custom_users (what the apps show) and back_custom_users (what orders and
// payments read). Email and the profile image aren't here: email is the sign-in (Supabase
// auth), and the image goes straight to Supabase Storage.

const MAX_USERNAME_CHARS: usize = 40;

#[derive(Deserialize)]
pub struct ProfileUpdate {
    pub username: String,
    pub phone: String,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Profile {
    pub username: String,
    // E.164, as stored.
    pub phone: String,
}

impl ProfileUpdate {
    // The phone rule is the app's and app_custom_users' CHECK: E.164, and a Nigerian number is
    // +234 then 10 digits starting 7, 8 or 9. Local forms (0803..., 803...) are accepted and
    // stored as E.164.
    fn validate(self) -> Result<Profile, ApiError> {
        let username = self.username.trim().to_string();
        let length = username.chars().count();
        if length == 0 || length > MAX_USERNAME_CHARS {
            return Err(ApiError::BadRequest(format!("username must be 1 to {} characters", MAX_USERNAME_CHARS)));
        }
        let phone = normalize_phone(&self.phone)
            .ok_or_else(|| ApiError::BadRequest("phone is not a valid phone number".into()))?;
        Ok(Profile { username, phone })
    }
}

// POST /riders/profile { username, phone } -> 200 { username, phone }
pub async fn update_profile(
    auth: AuthUser,
    pool: web::Data<DbPool>,
    body: web::Json<ProfileUpdate>,
) -> HttpResponse {
    let me = auth.user_id;
    let profile = match body.into_inner().validate() {
        Ok(profile) => profile,
        Err(e) => return e.response(),
    };

    let result = blocking(&pool, move |conn| {
        use diesel::result::{ Error, DatabaseErrorKind };

        let written = conn.transaction(|conn| {
            let updated = diesel::sql_query("UPDATE app_custom_users SET username = $2, phone = $3 WHERE id = $1")
                .bind::<SqlUuid, _>(me)
                .bind::<Text, _>(&profile.username)
                .bind::<Text, _>(&profile.phone)
                .execute(conn)?;
            if updated == 0 {
                return Ok(false);
            }
            // The backend copy, if the rider has one yet (created at signup by create-riders).
            diesel::sql_query("UPDATE back_custom_users SET name = $2, phone = $3 WHERE rider_id = $1")
                .bind::<SqlUuid, _>(me)
                .bind::<Text, _>(&profile.username)
                .bind::<Text, _>(&profile.phone)
                .execute(conn)?;
            Ok(true)
        });
        match written {
            Ok(true) => Ok(profile),
            Ok(false) => Err(ApiError::NotFound("No profile for this account".into())),
            Err(Error::DatabaseError(DatabaseErrorKind::UniqueViolation, _)) =>
                Err(ApiError::Conflict("That phone number is already used by another account".into())),
            Err(Error::DatabaseError(DatabaseErrorKind::CheckViolation, info)) =>
                Err(ApiError::BadRequest(format!("Profile refused: {}", info.message()))),
            Err(e) => Err(e.into()),
        }
    }).await;

    match result {
        Ok(profile) => HttpResponse::Ok().json(profile),
        Err(e) => e.response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(username: &str, phone: &str) -> Result<Profile, ApiError> {
        ProfileUpdate { username: username.into(), phone: phone.into() }.validate()
    }

    #[test]
    fn a_profile_is_trimmed_and_the_phone_stored_as_e164() {
        assert_eq!(
            update("  Ada  ", "0803 123 4567").ok(),
            Some(Profile { username: "Ada".into(), phone: "+2348031234567".into() })
        );
        assert!(update("Ada", "+447911123456").is_ok());
    }

    #[test]
    fn bad_names_and_numbers_are_refused() {
        assert!(update("   ", "+2348031234567").is_err());
        assert!(update(&"a".repeat(41), "+2348031234567").is_err());
        assert!(update(&"é".repeat(40), "+2348031234567").is_ok()); // characters, not bytes
        assert!(update("Ada", "+2340123456789").is_err()); // +234 must start 7, 8 or 9
        assert!(update("Ada", "+234803123456").is_err()); // 9 digits
        assert!(update("Ada", "12345").is_err());
    }
}
