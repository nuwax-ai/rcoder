use anyhow::{Context, Result};
use sqlx::{SqlitePool, migrate::MigrateDatabase};
use tracing::info;

pub struct DatabaseManager {
    pool: SqlitePool,
}

impl DatabaseManager {
    pub async fn new(database_url: &str) -> Result<Self> {
        info!("Initializing database: {}", database_url);

        // Create database if it doesn't exist
        if !sqlx::Sqlite::database_exists(database_url).await.unwrap_or(false) {
            sqlx::Sqlite::create_database(database_url).await
                .context("Failed to create database")?;
            info!("Database created: {}", database_url);
        }

        let pool = SqlitePool::connect(database_url).await
            .context("Failed to connect to database")?;

        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn run_migrations(&self) -> Result<()> {
        info!("Running database migrations");

        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("Failed to run database migrations")?;

        info!("Database migrations completed successfully");
        Ok(())
    }

    pub fn get_pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn close(&self) -> Result<()> {
        info!("Closing database connection");
        self.pool.close().await;
        Ok(())
    }
}