use crate::core::config::pie_home;
use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

pub type DbPool = Pool<SqliteConnectionManager>;

mod embedded {
    use refinery::embed_migrations;
    embed_migrations!("./src/core/db/migrations");
}

pub fn create_pool() -> Result<DbPool> {
    let home = pie_home();
    let db_path = home.join("pie.db");
    std::fs::create_dir_all(&home)?;

    let mut conn = rusqlite::Connection::open(&db_path)?;
    embedded::migrations::runner().run(&mut conn)?;
    drop(conn);

    let manager = SqliteConnectionManager::file(&db_path);
    let pool = Pool::builder().max_size(4).build(manager)?;
    Ok(pool)
}

#[cfg(test)]
pub fn create_test_pool() -> DbPool {
    let manager = SqliteConnectionManager::memory().with_init(|conn| {
        embedded::migrations::runner()
            .run(conn)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        Ok(())
    });
    Pool::builder().max_size(1).build(manager).unwrap()
}
