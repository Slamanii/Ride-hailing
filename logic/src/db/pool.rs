use diesel::pg::PgConnection;
use diesel::r2d2::{ self, ConnectionManager, CustomizeConnection };

pub type DbPool = r2d2::Pool<ConnectionManager<PgConnection>>;

// Built for Supabase's transaction pooler (Supavisor, port 6543), which hands each transaction
// to whatever server connection is free. So: no named prepared statements (see below) and no
// session state (SET, advisory locks, LISTEN); everything here runs per statement or inside one
// transaction, which the pooler keeps on a single server connection.
pub fn init_pool(database_url: &str) -> Result<DbPool, r2d2::PoolError> {
    let manager = ConnectionManager::<PgConnection>::new(database_url);
    r2d2::Pool::builder()
        .connection_customizer(Box::new(DisablePreparedStatements))
        .max_size(5)
        .build(manager)
}

// Diesel caches queries as named prepared statements on the connection. Behind a transaction
// pooler the next query may land on a server connection that never saw the statement
// ("prepared statement __diesel_stmt_N does not exist / already exists"). With the cache off,
// every query is sent as an unnamed statement, which poolers pass through safely.
#[derive(Debug)]
struct DisablePreparedStatements;

impl CustomizeConnection<PgConnection, diesel::r2d2::Error> for DisablePreparedStatements {
    fn on_acquire(&self, conn: &mut PgConnection) -> Result<(), diesel::r2d2::Error> {
        use diesel::connection::{ CacheSize, Connection };
        conn.set_prepared_statement_cache_size(CacheSize::Disabled);
        Ok(())
    }
}
