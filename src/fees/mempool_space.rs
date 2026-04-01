use bdk::FeeRate;
use governor::{
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter,
};
use serde::{Deserialize, Serialize};
use tracing::instrument;

use super::error::FeeEstimationError;
use crate::primitives::TxPriority;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecommendedFeesResponse {
    fastest_fee: u64,
    half_hour_fee: u64,
    hour_fee: u64,
    // economy_fee: u64,
    // minimum_fee: u64,
}

#[derive(Clone, Debug)]
pub struct MempoolSpaceClient {
    config: MempoolSpaceConfig,
    limiter: std::sync::Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
    client: reqwest_middleware::ClientWithMiddleware,
}

impl MempoolSpaceClient {
    pub fn new(config: MempoolSpaceConfig) -> Self {
        let rate_limit_per_second = super::non_zero_rate_limit_value(
            config.rate_limit_per_second,
            "mempool_space",
            "rate_limit_per_second",
        );
        let rate_limit_burst = super::non_zero_rate_limit_value(
            config.rate_limit_burst,
            "mempool_space",
            "rate_limit_burst",
        );
        let limiter = std::sync::Arc::new(RateLimiter::direct(
            Quota::per_second(rate_limit_per_second).allow_burst(rate_limit_burst),
        ));
        let client = super::build_http_client(config.timeout, config.number_of_retries);

        Self {
            config,
            limiter,
            client,
        }
    }

    #[instrument(name = "mempool_space.fee_rate", skip(self), ret, err)]
    pub async fn fee_rate(&self, priority: TxPriority) -> Result<FeeRate, FeeEstimationError> {
        self.limiter.until_ready().await;

        let url = format!("{}{}", self.config.url, "/api/v1/fees/recommended");
        let resp = self.client.get(&url).send().await?;
        let status = resp.status();
        let fee_estimations = resp
            .json::<RecommendedFeesResponse>()
            .await
            .map_err(|err| {
                tracing::warn!(
                    status = %status,
                    error = %err,
                    "mempool_space fee estimation response decode failed"
                );
                FeeEstimationError::CouldNotDecodeResponseBody(err)
            })?;
        match priority {
            TxPriority::HalfHour => Ok(FeeRate::from_sat_per_vb(
                fee_estimations.half_hour_fee as f32,
            )),
            TxPriority::OneHour => Ok(FeeRate::from_sat_per_vb(fee_estimations.hour_fee as f32)),
            TxPriority::NextBlock => {
                Ok(FeeRate::from_sat_per_vb(fee_estimations.fastest_fee as f32))
            }
        }
    }
}

#[serde_with::serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MempoolSpaceConfig {
    #[serde(default = "default_url")]
    pub url: String,
    #[serde_as(as = "serde_with::DurationSeconds<u64>")]
    #[serde(default = "default_timeout")]
    pub timeout: std::time::Duration,
    #[serde(default = "default_number_of_retries")]
    pub number_of_retries: u32,
    #[serde(default = "default_rate_limit_per_second")]
    pub rate_limit_per_second: u32,
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,
}

impl Default for MempoolSpaceConfig {
    fn default() -> Self {
        Self {
            url: default_url(),
            timeout: default_timeout(),
            number_of_retries: default_number_of_retries(),
            rate_limit_per_second: default_rate_limit_per_second(),
            rate_limit_burst: default_rate_limit_burst(),
        }
    }
}

fn default_url() -> String {
    "https://mempool.tk7.mempool.space".to_string()
}

fn default_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(3)
}

fn default_number_of_retries() -> u32 {
    2
}

fn default_rate_limit_per_second() -> u32 {
    2
}

fn default_rate_limit_burst() -> u32 {
    4
}
