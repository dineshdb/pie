use crate::config::pie_home;
use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

pub type DbPool = Pool<SqliteConnectionManager>;

mod embedded {
    use refinery::embed_migrations;
    embed_migrations!("./src/db/migrations");
}

/// Run migrations on a connection.
fn migrate(conn: &mut rusqlite::Connection) -> Result<(), refinery::Error> {
    embedded::migrations::runner().run(conn)?;
    Ok(())
}

/// Create an in-memory database (default).
pub fn create_memory_pool() -> Result<DbPool> {
    let manager = SqliteConnectionManager::memory().with_init(|conn| {
        migrate(conn).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        Ok(())
    });
    let pool = Pool::builder().max_size(4).build(manager)?;
    Ok(pool)
}

/// Create a persistent file-backed database.
pub fn create_persistent_pool() -> Result<DbPool> {
    let home = pie_home();
    let db_path = home.join("pie.db");
    std::fs::create_dir_all(&home)?;

    let mut conn = rusqlite::Connection::open(&db_path)?;
    migrate(&mut conn).map_err(|e| anyhow::anyhow!("migration failed: {e}"))?;
    drop(conn);

    let manager = SqliteConnectionManager::file(&db_path);
    let pool = Pool::builder().max_size(4).build(manager)?;
    Ok(pool)
}

/// Create database pool. Uses in-memory unless `persistent` is true.
pub fn create_pool(persistent: bool) -> Result<DbPool> {
    if persistent {
        create_persistent_pool()
    } else {
        create_memory_pool()
    }
}

#[cfg(test)]
pub fn create_test_pool() -> Result<DbPool> {
    create_memory_pool()
}
