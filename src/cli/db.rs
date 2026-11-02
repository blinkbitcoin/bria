use serde::{Deserialize, Serialize};
use sqlx::postgres::PgConnectOptions;
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbConfig {
    #[serde(default)]
    pub pg_con: String,
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,
    #[serde(default = "bool_true")]
    pub migrate_on_start: bool,
    // Postgres ends sessions left idle inside a transaction this long; 0 disables.
    #[serde(default = "default_idle_in_transaction_timeout_secs")]
    pub idle_in_transaction_timeout_secs: u64,
}

pub async fn init_pool(config: &DbConfig) -> anyhow::Result<sqlx::PgPool> {
    let connect_options = PgConnectOptions::from_str(&config.pg_con)?.options([(
        "idle_in_transaction_session_timeout",
        format!("{}s", config.idle_in_transaction_timeout_secs),
    )]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(config.pool_size)
        .connect_with(connect_options)
        .await?;
    if config.migrate_on_start {
        sqlx::migrate!().run(&pool).await?;
    }
    Ok(pool)
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            pg_con: "".to_string(),
            pool_size: default_pool_size(),
            migrate_on_start: true,
            idle_in_transaction_timeout_secs: default_idle_in_transaction_timeout_secs(),
        }
    }
}

fn default_pool_size() -> u32 {
    20
}

fn default_idle_in_transaction_timeout_secs() -> u64 {
    600
}

fn bool_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_in_transaction_session_is_terminated() -> anyhow::Result<()> {
        let pg_host = std::env::var("PG_HOST").unwrap_or("localhost".to_string());
        let config = DbConfig {
            pg_con: format!("postgres://user:password@{pg_host}:5432/pg"),
            pool_size: 1,
            migrate_on_start: false,
            idle_in_transaction_timeout_secs: 1,
        };
        let pool = init_pool(&config).await?;

        let (timeout,): (String,) = sqlx::query_as("SHOW idle_in_transaction_session_timeout")
            .fetch_one(&pool)
            .await?;
        assert_eq!(timeout, "1s");

        let mut tx = pool.begin().await?;
        sqlx::query("SELECT 1").execute(&mut *tx).await?;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert!(sqlx::query("SELECT 1").execute(&mut *tx).await.is_err());
        Ok(())
    }
}
