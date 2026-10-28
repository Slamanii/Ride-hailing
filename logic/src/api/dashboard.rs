use actix_web::{ web, HttpResponse };
use serde::Deserialize;
use serde_json::{ json, Value };
use diesel::prelude::*;
use diesel::pg::PgConnection;
use diesel::sql_types::{ BigInt, Jsonb, Text };
use chrono::{ Datelike, Duration as Days, NaiveDate, Utc };
use std::time::Duration;
use crate::api::errors::{ ApiError, blocking };
use crate::db::DbPool;
use crate::services::auth::AdminUser;
use crate::services::korapay;

// Admin dashboard (the website polls these). Read only; every number is worked out from the
// source tables except per-driver days, which back_driver_performance keeps (rebuilt below).
//   GET /admin/dashboard/live         right now: drivers, active orders, queues
//   GET /admin/dashboard/overview     a period: orders, money, trips, disputes, users, invest
//   GET /admin/dashboard/timeseries   a period in buckets, for charts
//   GET /admin/dashboard/drivers      a period, per driver
//   GET /admin/dashboard/assets       a period, per investment asset
// Periods: ?period=today|yesterday|7d|30d|this_month|last_month (default 7d), or
// ?from=YYYY-MM-DD&to=YYYY-MM-DD, days in Lagos time, both ends included.
// Auto-refresh: every response carries generated_at and refresh_secs, how often to poll it;
// refresh_secs is null for a period wholly in the past, which never changes.
// Money: _naira fields are order prices (naira), _kobo fields kobo.
// Admins only (AdminUser).

const LAGOS: &str = "Africa/Lagos";
const LIVE_REFRESH_SECS: u64 = 15;
const PERIOD_REFRESH_SECS: u64 = 60;
const TABLE_REFRESH_SECS: u64 = 300;
const MAX_PERIOD_DAYS: i64 = 731;
const MAX_HOURLY_DAYS: i64 = 31;

// ---------- Periods ----------

#[derive(Deserialize, Default)]
pub struct PeriodQuery {
    #[serde(default)]
    pub period: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

struct Period {
    from: NaiveDate,
    to: NaiveDate,
    // Includes today, so its numbers still move.
    live: bool,
}

impl Period {
    fn days(&self) -> i64 {
        (self.to - self.from).num_days() + 1
    }

    fn refresh(&self, secs: u64) -> Option<u64> {
        self.live.then_some(secs)
    }

    fn json(&self) -> Value {
        json!({ "from": self.from.to_string(), "to": self.to.to_string(), "days": self.days() })
    }
}

// Nigeria keeps UTC+1 all year.
fn lagos_today() -> NaiveDate {
    (Utc::now() + Days::hours(1)).date_naive()
}

fn parse_day(value: &str, field: &str) -> Result<NaiveDate, ApiError> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| ApiError::BadRequest(format!("{} must be a date like 2026-10-01", field)))
}

fn period_of(q: &PeriodQuery, today: NaiveDate) -> Result<Period, ApiError> {
    let (from, to) = match (q.from.as_deref(), q.to.as_deref()) {
        (Some(from), Some(to)) => (parse_day(from, "from")?, parse_day(to, "to")?),
        (Some(_), None) | (None, Some(_)) => return Err(ApiError::BadRequest("from and to go together".into())),
        (None, None) => {
            let first_of_month = today.with_day(1).unwrap_or(today);
            match q.period.as_deref().unwrap_or("7d") {
                "today" => (today, today),
                "yesterday" => (today - Days::days(1), today - Days::days(1)),
                "7d" => (today - Days::days(6), today),
                "30d" => (today - Days::days(29), today),
                "this_month" => (first_of_month, today),
                "last_month" => {
                    let last = first_of_month - Days::days(1);
                    (last.with_day(1).unwrap_or(last), last)
                }
                other => return Err(ApiError::BadRequest(format!(
                    "period '{}' isn't one of today, yesterday, 7d, 30d, this_month, last_month", other
                ))),
            }
        }
    };
    // Days that haven't happened have nothing in them.
    let to = to.min(today);
    if from > to {
        return Err(ApiError::BadRequest("from is after to".into()));
    }
    if (to - from).num_days() + 1 > MAX_PERIOD_DAYS {
        return Err(ApiError::BadRequest(format!("A period can cover at most {} days", MAX_PERIOD_DAYS)));
    }
    Ok(Period { from, to, live: to >= today })
}

#[derive(QueryableByName)]
struct JsonRow {
    #[diesel(sql_type = Jsonb)]
    data: Value,
}

// Runs a query that returns one jsonb column `data`, bound to the period's first and last day.
fn period_json(conn: &mut PgConnection, sql: &str, period: &Period) -> QueryResult<Value> {
    diesel::sql_query(sql)
        .bind::<Text, _>(period.from.to_string())
        .bind::<Text, _>(period.to.to_string())
        .get_result::<JsonRow>(conn)
        .map(|row| row.data)
}

fn respond(period: Option<&Period>, refresh_secs: Option<u64>, data: Result<Value, ApiError>) -> HttpResponse {
    match data {
        Ok(data) => HttpResponse::Ok().json(json!({
            "period": period.map(Period::json),
            "generated_at": Utc::now().to_rfc3339(),
            "refresh_secs": refresh_secs,
            "data": data,
        })),
        Err(e) => e.response(),
    }
}

// The period's start and end as instants: `b.lo` and `b.hi` once joined as `b`.
fn bounds() -> String {
    format!(
        "SELECT ($1::date)::timestamp AT TIME ZONE '{tz}' AS lo, ($2::date + 1)::timestamp AT TIME ZONE '{tz}' AS hi",
        tz = LAGOS
    )
}

// ---------- back_driver_performance ----------

// Rebuilds the rows of days $1..$2 from the source tables. Idempotent: a day is recomputed
// whole each time, so late completions, reviews and session ends land on the next pass.
fn rebuild_days(conn: &mut PgConnection, from: NaiveDate, to: NaiveDate) -> QueryResult<usize> {
    let sql = format!(
        "WITH b AS ({bounds}), \
         days AS ( \
           SELECT d::date AS day, d::timestamp AT TIME ZONE '{tz}' AS lo, (d::date + 1)::timestamp AT TIME ZONE '{tz}' AS hi \
             FROM generate_series($1::date, $2::date, interval '1 day') d), \
         offers AS ( \
           SELECT ro.driver_id, (ro.created_at AT TIME ZONE '{tz}')::date AS day, count(*) AS received, \
                  count(*) FILTER (WHERE ro.status = 'accepted') AS accepted, \
                  count(*) FILTER (WHERE ro.status = 'rejected') AS rejected, \
                  count(*) FILTER (WHERE ro.status = 'expired') AS expired \
             FROM app_ride_offers ro, b WHERE ro.created_at >= b.lo AND ro.created_at < b.hi \
            GROUP BY 1, 2), \
         trips AS ( \
           SELECT t.driver_id, (to_timestamp(t.end_ts) AT TIME ZONE '{tz}')::date AS day, \
                  count(*) FILTER (WHERE t.status = 'Completed') AS completed, \
                  count(*) FILTER (WHERE t.status = 'Cancelled') AS cancelled, \
                  COALESCE(sum(t.distance_km) FILTER (WHERE t.status = 'Completed'), 0) AS distance_km, \
                  COALESCE(sum(COALESCE(t.fare_estimate, 0)) FILTER (WHERE t.status = 'Completed'), 0) * 100 AS gross, \
                  COALESCE(sum(COALESCE(t.fare_estimate, 0) * CASE WHEN t.asset_id IS NOT NULL THEN $3 ELSE $4 END) \
                           FILTER (WHERE t.status = 'Completed'), 0) / 100 AS earnings, \
                  COALESCE(sum(COALESCE(t.fare_estimate, 0) * $5) \
                           FILTER (WHERE t.status = 'Completed' AND t.asset_id IS NOT NULL), 0) / 100 AS asset_pool, \
                  mode() WITHIN GROUP (ORDER BY t.asset_id) AS asset_id \
             FROM back_trips t, b \
            WHERE t.end_ts IS NOT NULL AND to_timestamp(t.end_ts) >= b.lo AND to_timestamp(t.end_ts) < b.hi \
            GROUP BY 1, 2), \
         disputes AS ( \
           SELECT x.driver_id, (x.created_at AT TIME ZONE '{tz}')::date AS day, count(*) AS raised, \
                  count(*) FILTER (WHERE x.status = 'upheld') AS upheld, \
                  count(*) FILTER (WHERE x.status = 'rejected') AS rejected \
             FROM back_package_disputes x, b WHERE x.created_at >= b.lo AND x.created_at < b.hi \
            GROUP BY 1, 2), \
         online AS ( \
           SELECT s.driver_id, dy.day, \
                  sum(extract(epoch FROM least(COALESCE(s.ended_at, s.last_seen_at), dy.hi) \
                                       - greatest(s.started_at, dy.lo))) / 60 AS minutes \
             FROM back_driver_sessions s \
             JOIN days dy ON s.started_at < dy.hi AND COALESCE(s.ended_at, s.last_seen_at) > dy.lo \
            GROUP BY 1, 2), \
         keys AS ( \
           SELECT driver_id, day FROM offers UNION SELECT driver_id, day FROM trips \
           UNION SELECT driver_id, day FROM disputes UNION SELECT driver_id, day FROM online) \
         INSERT INTO back_driver_performance \
                (driver_id, day, asset_id, offers_received, offers_accepted, offers_rejected, offers_expired, \
                 trips_completed, trips_cancelled, distance_km, gross_fares, driver_earnings, asset_pool, \
                 disputes_raised, disputes_upheld, disputes_rejected, online_minutes, updated_at) \
         SELECT k.driver_id, k.day, \
                COALESCE(t.asset_id, \
                  (SELECT aa.asset_id FROM asset_assignments aa JOIN days dy ON dy.day = k.day \
                    WHERE aa.driver_id = k.driver_id AND aa.started_at < dy.hi \
                      AND (aa.ended_at IS NULL OR aa.ended_at > dy.lo) \
                    ORDER BY aa.started_at DESC LIMIT 1)), \
                COALESCE(o.received, 0)::int4, COALESCE(o.accepted, 0)::int4, \
                COALESCE(o.rejected, 0)::int4, COALESCE(o.expired, 0)::int4, \
                COALESCE(t.completed, 0)::int4, COALESCE(t.cancelled, 0)::int4, \
                COALESCE(t.distance_km, 0)::numeric(10,2), COALESCE(t.gross, 0)::int8, \
                floor(COALESCE(t.earnings, 0))::int8, floor(COALESCE(t.asset_pool, 0))::int8, \
                COALESCE(d.raised, 0)::int4, COALESCE(d.upheld, 0)::int4, COALESCE(d.rejected, 0)::int4, \
                round(COALESCE(n.minutes, 0))::int4, now() \
           FROM keys k \
           LEFT JOIN offers o ON o.driver_id = k.driver_id AND o.day = k.day \
           LEFT JOIN trips t ON t.driver_id = k.driver_id AND t.day = k.day \
           LEFT JOIN disputes d ON d.driver_id = k.driver_id AND d.day = k.day \
           LEFT JOIN online n ON n.driver_id = k.driver_id AND n.day = k.day \
         ON CONFLICT (driver_id, day) DO UPDATE SET \
                asset_id = EXCLUDED.asset_id, offers_received = EXCLUDED.offers_received, \
                offers_accepted = EXCLUDED.offers_accepted, offers_rejected = EXCLUDED.offers_rejected, \
                offers_expired = EXCLUDED.offers_expired, trips_completed = EXCLUDED.trips_completed, \
                trips_cancelled = EXCLUDED.trips_cancelled, distance_km = EXCLUDED.distance_km, \
                gross_fares = EXCLUDED.gross_fares, driver_earnings = EXCLUDED.driver_earnings, \
                asset_pool = EXCLUDED.asset_pool, disputes_raised = EXCLUDED.disputes_raised, \
                disputes_upheld = EXCLUDED.disputes_upheld, disputes_rejected = EXCLUDED.disputes_rejected, \
                online_minutes = EXCLUDED.online_minutes, updated_at = now()",
        bounds = bounds(), tz = LAGOS
    );
    diesel::sql_query(sql)
        .bind::<Text, _>(from.to_string())
        .bind::<Text, _>(to.to_string())
        .bind::<BigInt, _>(korapay::driver_share_bps(true))
        .bind::<BigInt, _>(korapay::driver_share_bps(false))
        .bind::<BigInt, _>(korapay::ASSET_POOL_BPS)
        .execute(conn)
}

#[derive(QueryableByName)]
struct HasRows {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    has_rows: bool,
}

// The first pass fills the last BACKFILL_DAYS; after that only yesterday and today change.
const BACKFILL_DAYS: i64 = 90;
const PERFORMANCE_EVERY_SECS: u64 = 300;

async fn refresh_performance(pool: &web::Data<DbPool>) {
    let today = lagos_today();
    let result = blocking(pool, move |conn| {
        let filled = diesel::sql_query("SELECT EXISTS (SELECT 1 FROM back_driver_performance) AS has_rows")
            .get_result::<HasRows>(conn)?
            .has_rows;
        let from = today - Days::days(if filled { 1 } else { BACKFILL_DAYS });
        Ok(rebuild_days(conn, from, today)?)
    }).await;
    if let Err(e) = result {
        eprintln!("Dashboard: driver performance not rebuilt: {}", e.message());
    }
}

pub fn spawn_performance_job(pool: web::Data<DbPool>) {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_secs(PERFORMANCE_EVERY_SECS));
        loop {
            ticks.tick().await;
            refresh_performance(&pool).await;
        }
    });
}

// ---------- Live ----------

pub async fn live(_admin: AdminUser, pool: web::Data<DbPool>) -> HttpResponse {
    let data = blocking(&pool, |conn| Ok(diesel::sql_query(
        "SELECT jsonb_build_object( \
           'drivers', (SELECT jsonb_build_object( \
               'available', count(*) FILTER (WHERE status = 'available'), \
               'busy', count(*) FILTER (WHERE status = 'busy'), \
               'offline', count(*) FILTER (WHERE status = 'offline'), \
               'location_stale', count(*) FILTER (WHERE status <> 'offline' \
                   AND (location_updated_at IS NULL OR location_updated_at < now() - interval '10 minutes'))) \
             FROM back_drivers), \
           'online_sessions', (SELECT count(*) FROM back_driver_sessions \
             WHERE ended_at IS NULL AND last_seen_at > now() - interval '5 minutes'), \
           'orders', (SELECT COALESCE(jsonb_object_agg(status, n), '{}'::jsonb) FROM ( \
               SELECT status::text AS status, count(*) AS n FROM app_delivery_orders \
                WHERE status::text IN ('awaiting_payment', 'pending', 'no_driver', 'arriving_pickup', \
                                       'driver_arrived', 'in_transit') \
                GROUP BY 1) x), \
           'returning', (SELECT count(*) FROM app_returned WHERE status = 'returning'), \
           'payouts', (SELECT jsonb_build_object( \
               'awaiting', count(*) FILTER (WHERE status = 'AwaitingPayout'), \
               'failed', count(*) FILTER (WHERE status = 'PayoutFailed')) FROM back_trips), \
           'queues', jsonb_build_object( \
               'disputes', (SELECT count(*) FROM back_package_disputes WHERE status = 'pending'), \
               'dispute_refunds_failed', (SELECT count(*) FROM back_package_disputes WHERE refund_status = 'failed'), \
               'transfers', (SELECT count(*) FROM invest_slot_transfers WHERE status = 'requested'), \
               'applications', (SELECT count(*) FROM driver_applications WHERE status IN ('submitted', 'contacted')), \
               'withdrawals', (SELECT count(*) FROM invest_withdrawals WHERE status IN ('pending', 'processing')), \
               'purchases_open', (SELECT count(*) FROM back_invest_purchases WHERE status = 'pending' AND expires_at > now())) \
         ) AS data"
    ).get_result::<JsonRow>(conn)?.data)).await;
    respond(None, Some(LIVE_REFRESH_SECS), data)
}

// ---------- Overview ----------

pub async fn overview(_admin: AdminUser, pool: web::Data<DbPool>, query: web::Query<PeriodQuery>) -> HttpResponse {
    let period = match period_of(&query, lagos_today()) {
        Ok(p) => p,
        Err(e) => return e.response(),
    };
    let sql = format!(
        "WITH b AS ({bounds}), \
         created AS (SELECT o.status::text AS status, o.source FROM app_delivery_orders o, b \
                      WHERE o.created_at >= b.lo AND o.created_at < b.hi), \
         delivered AS (SELECT o.source, o.price, o.insurance FROM app_delivery_orders o, b \
                        WHERE o.status = 'delivered' AND o.dropoff_time >= b.lo AND o.dropoff_time < b.hi), \
         cancelled AS (SELECT o.cancelled_by, o.client_id, o.driver_id, o.is_pickup_code_authenticated \
                         FROM app_delivery_orders o, b \
                        WHERE o.status = 'cancelled' AND o.cancelled_at >= b.lo AND o.cancelled_at < b.hi), \
         perf AS (SELECT * FROM back_driver_performance WHERE day BETWEEN $1::date AND $2::date) \
         SELECT jsonb_build_object( \
           'orders', jsonb_build_object( \
               'created', (SELECT count(*) FROM created), \
               'by_status', (SELECT COALESCE(jsonb_object_agg(status, n), '{{}}'::jsonb) \
                               FROM (SELECT status, count(*) AS n FROM created GROUP BY 1) x), \
               'by_source', (SELECT COALESCE(jsonb_object_agg(source, n), '{{}}'::jsonb) \
                               FROM (SELECT source, count(*) AS n FROM created GROUP BY 1) x)), \
           'deliveries', (SELECT jsonb_build_object( \
               'count', count(*), \
               'app_charged_naira', COALESCE(sum(price) FILTER (WHERE source = 'app'), 0), \
               'insurance_naira', COALESCE(sum(insurance), 0)) FROM delivered), \
           'cancellations', (SELECT jsonb_build_object( \
               'total', count(*), \
               'by_customer', count(*) FILTER (WHERE cancelled_by = client_id), \
               'by_driver', count(*) FILTER (WHERE cancelled_by = driver_id), \
               'by_system', count(*) FILTER (WHERE cancelled_by IS NULL), \
               'after_pickup', count(*) FILTER (WHERE is_pickup_code_authenticated)) FROM cancelled), \
           'trips', (SELECT jsonb_build_object( \
               'completed', COALESCE(sum(trips_completed), 0), \
               'cancelled', COALESCE(sum(trips_cancelled), 0), \
               'distance_km', COALESCE(sum(distance_km), 0), \
               'gross_fares_kobo', COALESCE(sum(gross_fares), 0), \
               'driver_earnings_kobo', COALESCE(sum(driver_earnings), 0), \
               'asset_pool_kobo', COALESCE(sum(asset_pool), 0), \
               'treasury_kobo', COALESCE(sum(gross_fares - driver_earnings - asset_pool), 0), \
               'online_minutes', COALESCE(sum(online_minutes), 0), \
               'active_drivers', count(DISTINCT driver_id) FILTER (WHERE trips_completed > 0 OR online_minutes > 0)) \
             FROM perf), \
           'offers', (SELECT jsonb_build_object( \
               'received', COALESCE(sum(offers_received), 0), \
               'accepted', COALESCE(sum(offers_accepted), 0), \
               'rejected', COALESCE(sum(offers_rejected), 0), \
               'expired', COALESCE(sum(offers_expired), 0)) FROM perf), \
           'disputes', (SELECT jsonb_build_object( \
               'raised', count(*), \
               'pending', count(*) FILTER (WHERE status = 'pending'), \
               'upheld', count(*) FILTER (WHERE status = 'upheld'), \
               'rejected', count(*) FILTER (WHERE status = 'rejected')) \
             FROM back_package_disputes x, b WHERE x.created_at >= b.lo AND x.created_at < b.hi), \
           'users', jsonb_build_object( \
               'new', (SELECT count(*) FROM app_custom_users u, b WHERE u.created_at >= b.lo AND u.created_at < b.hi)), \
           'applications', (SELECT COALESCE(jsonb_object_agg(status, n), '{{}}'::jsonb) FROM ( \
               SELECT a.status, count(*) AS n FROM driver_applications a, b \
                WHERE a.created_at >= b.lo AND a.created_at < b.hi GROUP BY 1) x), \
           'invest', jsonb_build_object( \
               'purchases', (SELECT jsonb_build_object('count', count(*), \
                                    'slots', COALESCE(sum(cardinality(slot_ids)), 0), \
                                    'amount_kobo', COALESCE(sum(amount), 0)) \
                               FROM back_invest_purchases p, b \
                              WHERE p.status = 'paid' AND p.paid_at >= b.lo AND p.paid_at < b.hi), \
               'withdrawals_requested', (SELECT jsonb_build_object('count', count(*), 'amount_kobo', COALESCE(sum(amount), 0)) \
                               FROM invest_withdrawals w, b WHERE w.requested_at >= b.lo AND w.requested_at < b.hi), \
               'withdrawals_paid', (SELECT jsonb_build_object('count', count(*), 'amount_kobo', COALESCE(sum(amount), 0)) \
                               FROM invest_withdrawals w, b \
                              WHERE w.status = 'paid' AND w.paid_at >= b.lo AND w.paid_at < b.hi), \
               'earnings_credited_kobo', (SELECT COALESCE(sum(amount), 0) FROM invest_earnings e \
                                           WHERE e.week_start BETWEEN $1::date AND $2::date)) \
         ) AS data",
        bounds = bounds()
    );
    let refresh = period.refresh(PERIOD_REFRESH_SECS);
    let (from, to) = (period.from, period.to);
    let data = blocking(&pool, move |conn| Ok(period_json(conn, &sql, &Period { from, to, live: false })?)).await;
    respond(Some(&period), refresh, data)
}

// ---------- Timeseries ----------

#[derive(Deserialize)]
pub struct SeriesQuery {
    #[serde(flatten)]
    pub period: PeriodQuery,
    // orders (created), revenue (app deliveries, naira), trips (completed), cancellations
    pub metric: String,
    // hour, day (default), week, month
    #[serde(default)]
    pub bucket: Option<String>,
}

// Each metric as (when, value) rows; only these fixed strings ever reach the SQL.
fn series_source(metric: &str) -> Option<&'static str> {
    Some(match metric {
        "orders" => "SELECT o.created_at AS at, 1::numeric AS v FROM app_delivery_orders o",
        "revenue" => "SELECT o.dropoff_time AS at, COALESCE(o.price, 0) AS v FROM app_delivery_orders o \
                      WHERE o.status = 'delivered' AND o.source = 'app'",
        "trips" => "SELECT to_timestamp(t.end_ts) AS at, 1::numeric AS v FROM back_trips t \
                    WHERE t.status = 'Completed' AND t.end_ts IS NOT NULL",
        "cancellations" => "SELECT o.cancelled_at AS at, 1::numeric AS v FROM app_delivery_orders o \
                            WHERE o.status = 'cancelled'",
        _ => return None,
    })
}

pub async fn timeseries(_admin: AdminUser, pool: web::Data<DbPool>, query: web::Query<SeriesQuery>) -> HttpResponse {
    let query = query.into_inner();
    let period = match period_of(&query.period, lagos_today()) {
        Ok(p) => p,
        Err(e) => return e.response(),
    };
    let Some(source) = series_source(&query.metric) else {
        return ApiError::BadRequest("metric must be orders, revenue, trips or cancellations".into()).response();
    };
    let bucket = query.bucket.as_deref().unwrap_or("day");
    if !["hour", "day", "week", "month"].contains(&bucket) {
        return ApiError::BadRequest("bucket must be hour, day, week or month".into()).response();
    }
    if bucket == "hour" && period.days() > MAX_HOURLY_DAYS {
        return ApiError::BadRequest(format!("Hourly buckets cover at most {} days", MAX_HOURLY_DAYS)).response();
    }

    // Buckets are Lagos local time; empty ones are 0, so the chart has no gaps.
    let sql = format!(
        "WITH b AS ({bounds}), \
         ev AS ({source}), \
         series AS (SELECT generate_series(date_trunc('{bucket}', ($1::date)::timestamp), \
                                           date_trunc('{bucket}', ($2::date)::timestamp), \
                                           interval '1 {bucket}') AS t), \
         agg AS (SELECT date_trunc('{bucket}', ev.at AT TIME ZONE '{tz}') AS t, sum(ev.v) AS v \
                   FROM ev, b WHERE ev.at >= b.lo AND ev.at < b.hi GROUP BY 1) \
         SELECT COALESCE(jsonb_agg(jsonb_build_object('t', s.t, 'value', COALESCE(a.v, 0)) ORDER BY s.t), '[]'::jsonb) AS data \
           FROM series s LEFT JOIN agg a ON a.t = s.t",
        bounds = bounds(), source = source, bucket = bucket, tz = LAGOS
    );
    // An hourly series' last day runs to 23:00, not 00:00.
    let sql = if bucket == "hour" {
        sql.replace("date_trunc('hour', ($2::date)::timestamp)", "($2::date)::timestamp + interval '23 hours'")
    } else {
        sql
    };
    let refresh = period.refresh(PERIOD_REFRESH_SECS);
    let (from, to) = (period.from, period.to);
    let data = blocking(&pool, move |conn| Ok(period_json(conn, &sql, &Period { from, to, live: false })?)).await;
    respond(Some(&period), refresh, data.map(|series| json!({ "metric": query.metric, "bucket": bucket, "series": series })))
}

// ---------- Drivers ----------

#[derive(Deserialize)]
pub struct DriversQuery {
    #[serde(flatten)]
    pub period: PeriodQuery,
    // trips (default), earnings, online, acceptance, disputes
    #[serde(default)]
    pub sort: Option<String>,
}

fn driver_order(sort: &str) -> Option<&'static str> {
    Some(match sort {
        "trips" => "trips_completed DESC, name",
        "earnings" => "driver_earnings_kobo DESC, name",
        "online" => "online_minutes DESC, name",
        "acceptance" => "acceptance_rate DESC NULLS LAST, name",
        "disputes" => "disputes_raised DESC, name",
        _ => return None,
    })
}

// Every driver, idle ones too. on_trip_minutes is worked out here from the trips themselves;
// utilisation is on-trip time over online time.
pub async fn drivers(_admin: AdminUser, pool: web::Data<DbPool>, query: web::Query<DriversQuery>) -> HttpResponse {
    let query = query.into_inner();
    let period = match period_of(&query.period, lagos_today()) {
        Ok(p) => p,
        Err(e) => return e.response(),
    };
    let Some(order) = driver_order(query.sort.as_deref().unwrap_or("trips")) else {
        return ApiError::BadRequest("sort must be trips, earnings, online, acceptance or disputes".into()).response();
    };
    let sql = format!(
        "WITH b AS ({bounds}), \
         perf AS ( \
           SELECT driver_id, sum(offers_received) AS offers_received, sum(offers_accepted) AS offers_accepted, \
                  sum(offers_rejected) AS offers_rejected, sum(offers_expired) AS offers_expired, \
                  sum(trips_completed) AS trips_completed, sum(trips_cancelled) AS trips_cancelled, \
                  sum(distance_km) AS distance_km, sum(gross_fares) AS gross_fares, \
                  sum(driver_earnings) AS driver_earnings, sum(disputes_raised) AS disputes_raised, \
                  sum(disputes_upheld) AS disputes_upheld, sum(disputes_rejected) AS disputes_rejected, \
                  sum(online_minutes) AS online_minutes, \
                  count(*) FILTER (WHERE trips_completed > 0 OR online_minutes > 0) AS active_days \
             FROM back_driver_performance WHERE day BETWEEN $1::date AND $2::date GROUP BY driver_id), \
         on_trip AS ( \
           SELECT t.driver_id, sum(t.end_ts - t.start_ts) / 60 AS minutes FROM back_trips t, b \
            WHERE t.end_ts IS NOT NULL AND t.end_ts > t.start_ts \
              AND to_timestamp(t.end_ts) >= b.lo AND to_timestamp(t.end_ts) < b.hi \
            GROUP BY 1), \
         rows AS ( \
           SELECT d.driver_id, d.name, d.phone, d.vehicle_type, d.status, d.tier, d.plate_number, \
                  (SELECT ia.code FROM asset_assignments aa JOIN invest_assets ia ON ia.id = aa.asset_id \
                    WHERE aa.driver_id = d.driver_id AND aa.ended_at IS NULL) AS current_asset, \
                  COALESCE(p.offers_received, 0) AS offers_received, COALESCE(p.offers_accepted, 0) AS offers_accepted, \
                  COALESCE(p.offers_rejected, 0) AS offers_rejected, COALESCE(p.offers_expired, 0) AS offers_expired, \
                  CASE WHEN p.offers_received > 0 THEN round(p.offers_accepted::numeric / p.offers_received, 3) END \
                    AS acceptance_rate, \
                  COALESCE(p.trips_completed, 0) AS trips_completed, COALESCE(p.trips_cancelled, 0) AS trips_cancelled, \
                  COALESCE(p.distance_km, 0) AS distance_km, COALESCE(p.gross_fares, 0) AS gross_fares_kobo, \
                  COALESCE(p.driver_earnings, 0) AS driver_earnings_kobo, \
                  COALESCE(p.disputes_raised, 0) AS disputes_raised, COALESCE(p.disputes_upheld, 0) AS disputes_upheld, \
                  COALESCE(p.disputes_rejected, 0) AS disputes_rejected, \
                  COALESCE(p.online_minutes, 0) AS online_minutes, COALESCE(ot.minutes, 0)::int8 AS on_trip_minutes, \
                  CASE WHEN p.online_minutes > 0 THEN round(COALESCE(ot.minutes, 0)::numeric / p.online_minutes, 3) END \
                    AS utilisation, \
                  COALESCE(p.active_days, 0) AS active_days \
             FROM back_drivers d \
             LEFT JOIN perf p ON p.driver_id = d.driver_id \
             LEFT JOIN on_trip ot ON ot.driver_id = d.driver_id) \
         SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY r.rank), '[]'::jsonb) AS data \
           FROM (SELECT rows.*, row_number() OVER (ORDER BY {order}) AS rank FROM rows) r",
        bounds = bounds(), order = order
    );
    let refresh = period.refresh(TABLE_REFRESH_SECS);
    let (from, to) = (period.from, period.to);
    let data = blocking(&pool, move |conn| Ok(period_json(conn, &sql, &Period { from, to, live: false })?)).await;
    respond(Some(&period), refresh, data.map(|drivers| json!({ "drivers": drivers })))
}

// ---------- Assets ----------

// Every investment asset, with the period's work summed from its drivers' days.
pub async fn assets(_admin: AdminUser, pool: web::Data<DbPool>, query: web::Query<PeriodQuery>) -> HttpResponse {
    let period = match period_of(&query, lagos_today()) {
        Ok(p) => p,
        Err(e) => return e.response(),
    };
    let sql = "WITH perf AS ( \
           SELECT asset_id, sum(trips_completed) AS trips, sum(distance_km) AS distance_km, \
                  sum(gross_fares) AS gross, sum(asset_pool) AS pool, sum(online_minutes) AS online, \
                  count(DISTINCT driver_id) AS drivers, count(*) FILTER (WHERE trips_completed > 0) AS active_days \
             FROM back_driver_performance \
            WHERE day BETWEEN $1::date AND $2::date AND asset_id IS NOT NULL GROUP BY asset_id) \
         SELECT COALESCE(jsonb_agg(to_jsonb(x) ORDER BY x.code), '[]'::jsonb) AS data FROM ( \
           SELECT a.id AS asset_id, a.code, a.status, a.vehicle_name, a.plate_number, \
                  pl.name AS plan, pl.asset_type, a.funding_ends_at, a.activated_at, \
                  (SELECT jsonb_build_object( \
                      'open', count(*) FILTER (WHERE holder = 'open'), \
                      'investor', count(*) FILTER (WHERE holder = 'investor'), \
                      'business', count(*) FILTER (WHERE holder = 'business')) \
                     FROM invest_slots s WHERE s.asset_id = a.id) AS slots, \
                  (SELECT dr.name FROM asset_assignments aa JOIN back_drivers dr ON dr.driver_id = aa.driver_id \
                    WHERE aa.asset_id = a.id AND aa.ended_at IS NULL) AS current_driver, \
                  COALESCE(p.trips, 0) AS trips_completed, COALESCE(p.distance_km, 0) AS distance_km, \
                  COALESCE(p.gross, 0) AS gross_fares_kobo, COALESCE(p.pool, 0) AS investor_pool_kobo, \
                  COALESCE(p.online, 0) AS online_minutes, COALESCE(p.drivers, 0) AS drivers, \
                  COALESCE(p.active_days, 0) AS active_days \
             FROM invest_assets a \
             JOIN invest_plans pl ON pl.id = a.plan_id \
             LEFT JOIN perf p ON p.asset_id = a.id) x";
    let refresh = period.refresh(TABLE_REFRESH_SECS);
    let (from, to) = (period.from, period.to);
    let data = blocking(&pool, move |conn| Ok(period_json(conn, sql, &Period { from, to, live: false })?)).await;
    respond(Some(&period), refresh, data.map(|assets| json!({ "assets": assets })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(period: Option<&str>, from: Option<&str>, to: Option<&str>) -> PeriodQuery {
        PeriodQuery { period: period.map(Into::into), from: from.map(Into::into), to: to.map(Into::into) }
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn presets_end_today_and_include_both_ends() {
        let today = day("2026-10-22");
        let p = period_of(&q(None, None, None), today).ok().unwrap();
        assert_eq!((p.from, p.to, p.days(), p.live), (day("2026-10-16"), today, 7, true));
        let p = period_of(&q(Some("this_month"), None, None), today).ok().unwrap();
        assert_eq!((p.from, p.to), (day("2026-10-01"), today));
    }

    #[test]
    fn last_month_is_closed_and_needs_no_refresh() {
        let p = period_of(&q(Some("last_month"), None, None), day("2026-03-15")).ok().unwrap();
        assert_eq!((p.from, p.to, p.live), (day("2026-02-01"), day("2026-02-28"), false));
        assert_eq!(p.refresh(60), None);
    }

    #[test]
    fn custom_ranges_are_checked() {
        let today = day("2026-10-22");
        assert!(period_of(&q(None, Some("2026-10-01"), None), today).is_err());
        assert!(period_of(&q(None, Some("2026-10-10"), Some("2026-10-01")), today).is_err());
        assert!(period_of(&q(None, Some("2020-01-01"), Some("2026-10-01")), today).is_err());
        assert!(period_of(&q(Some("forever"), None, None), today).is_err());
        // A future end stops at today.
        let p = period_of(&q(None, Some("2026-10-20"), Some("2026-12-31")), today).ok().unwrap();
        assert_eq!((p.to, p.live), (today, true));
    }
}
