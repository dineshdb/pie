use crate::config::pie_home;
use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

pub type DbPool = sqlx::SqlitePool;

pub async fn create_persistent_pool() -> Result<DbPool> {
    let home = pie_home();
    let db_path = home.join("pie.db");
    std::fs::create_dir_all(&home)?;

    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    sqlx::migrate!("./src/db/migrations").run(&pool).await?;
    Ok(pool)
}

/// In-memory pool for tests. Gated behind `test-util` so downstream
/// crates' tests can use it without shipping test code in release builds.
#[cfg(any(test, feature = "test-util"))]
pub async fn create_test_pool() -> Result<DbPool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    sqlx::migrate!("./src/db/migrations").run(&pool).await?;
    Ok(pool)
}
