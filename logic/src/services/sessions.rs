use actix_web::web;
use diesel::prelude::*;
use diesel::pg::PgConnection;
use diesel::sql_types::{ Integer, Uuid as SqlUuid };
use uuid::Uuid;
use crate::api::errors::blocking;
use crate::db::DbPool;

// Driver online time (back_driver_sessions), counted from what the rider app already sends:
// a location ping every 5s/5m while moving and a heartbeat every 2 minutes when parked. A
// session is a run of pings with no gap over SESSION_GAP_MINS; it ends at the last ping
// received, so a dead phone or a killed app stops the clock without ever saying "offline".
// One row per session, not per ping. Accurate to about one heartbeat.
//   ping (process-geolocation)   extends the open session, or closes a stale one and opens anew
//   update-status available       opens one if there's no fresh one
//   update-status offline         closes it now
//   sweep                         closes sessions whose pings stopped
// Every write here runs in a savepoint and only logs a failure: a location update or a status
// change never fails because of the clock.

const SESSION_GAP_MINS: i32 = 5;

// The driver is online now: extend the fresh open session, or start one. Offline drivers are
// skipped, so a ping still in flight after "go offline" doesn't reopen anything.
fn touch(conn: &mut PgConnection, driver: Uuid) -> QueryResult<()> {
    let extended = diesel::sql_query(
        "UPDATE back_driver_sessions SET last_seen_at = now() \
          WHERE driver_id = $1 AND ended_at IS NULL \
            AND last_seen_at > now() - make_interval(mins => $2) \
            AND EXISTS (SELECT 1 FROM back_drivers WHERE driver_id = $1 AND status <> 'offline')"
    )
    .bind::<SqlUuid, _>(driver)
    .bind::<Integer, _>(SESSION_GAP_MINS)
    .execute(conn)?;
    if extended > 0 {
        return Ok(());
    }

    // The gap itself isn't counted: a stale session ends at its last ping.
    close_stale(conn, Some(driver))?;
    diesel::sql_query(
        "INSERT INTO back_driver_sessions (driver_id) \
         SELECT $1 WHERE EXISTS (SELECT 1 FROM back_drivers WHERE driver_id = $1 AND status <> 'offline') \
         ON CONFLICT (driver_id) WHERE ended_at IS NULL DO NOTHING"
    )
    .bind::<SqlUuid, _>(driver)
    .execute(conn)?;
    Ok(())
}

// Ends open sessions with no ping for SESSION_GAP_MINS, at their last ping. One driver, or all.
fn close_stale(conn: &mut PgConnection, driver: Option<Uuid>) -> QueryResult<usize> {
    diesel::sql_query(
        "UPDATE back_driver_sessions SET ended_at = last_seen_at \
          WHERE ended_at IS NULL AND last_seen_at <= now() - make_interval(mins => $2) \
            AND ($1::uuid IS NULL OR driver_id = $1)"
    )
    .bind::<diesel::sql_types::Nullable<SqlUuid>, _>(driver)
    .bind::<Integer, _>(SESSION_GAP_MINS)
    .execute(conn)
}

// Going offline: ends the open session now (or at its last ping, if the pings had already stopped).
fn close(conn: &mut PgConnection, driver: Uuid) -> QueryResult<()> {
    diesel::sql_query(
        "UPDATE back_driver_sessions \
            SET ended_at = CASE WHEN last_seen_at > now() - make_interval(mins => $2) THEN now() ELSE last_seen_at END \
          WHERE driver_id = $1 AND ended_at IS NULL"
    )
    .bind::<SqlUuid, _>(driver)
    .bind::<Integer, _>(SESSION_GAP_MINS)
    .execute(conn)?;
    Ok(())
}

fn logged(conn: &mut PgConnection, driver: Uuid, what: &str, step: fn(&mut PgConnection, Uuid) -> QueryResult<()>) {
    if let Err(e) = conn.transaction(|conn| step(conn, driver)) {
        eprintln!("Driver {}: online session not {}: {}", driver, what, e);
    }
}

// A location ping from an online driver.
pub fn record_ping(conn: &mut PgConnection, driver: Uuid) {
    logged(conn, driver, "extended", touch);
}

// update-status: call after the status is written.
pub fn record_status(conn: &mut PgConnection, driver: Uuid, status: &str) {
    if status == "offline" {
        logged(conn, driver, "closed", close);
    } else {
        logged(conn, driver, "opened", touch);
    }
}

// From the periodic sweep.
pub async fn sweep_sessions(pool: &web::Data<DbPool>) {
    if let Err(e) = blocking(pool, |conn| Ok(close_stale(conn, None)?)).await {
        eprintln!("Session sweep: could not close stale sessions: {}", e.message());
    }
}
