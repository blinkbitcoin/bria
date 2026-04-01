use bdk::FeeRate;
use governor::{Quota, RateLimiter};
use serde::{Deserialize, Serialize};
use tracing::instrument;

use super::error::FeeEstimationError;
use crate::primitives::TxPriority;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeeEstimatesResponse {
    #[serde(rename = "1")]
    next_block: f32,
    #[serde(rename = "3")]
    half_hour_fee: f32,
    #[serde(rename = "6")]
    hour_fee: f32,
}

#[derive(Clone, Debug)]
pub struct BlockstreamClient {
    config: BlockstreamConfig,
    client: reqwest_middleware::ClientWithMiddleware,
}

impl BlockstreamClient {
    pub fn new(config: BlockstreamConfig) -> Self {
        let rate_limit_per_second = super::non_zero_rate_limit_value(
            config.rate_limit_per_second,
            "blockstream",
            "rate_limit_per_second",
        );
        let rate_limit_burst = super::non_zero_rate_limit_value(
            config.rate_limit_burst,
            "blockstream",
            "rate_limit_burst",
        );
        let limiter = std::sync::Arc::new(RateLimiter::direct(
            Quota::per_second(rate_limit_per_second).allow_burst(rate_limit_burst),
        ));
        let client = super::build_http_client(
            config.timeout,
            config.number_of_retries,
            "blockstream",
            limiter,
        );

        Self { config, client }
    }

    #[instrument(name = "blockstream.fee_rate", skip(self), ret, err)]
    pub async fn fee_rate(&self, priority: TxPriority) -> Result<FeeRate, FeeEstimationError> {
        let url = format!("{}{}", self.config.url, "/api/fee-estimates");
        let resp = self.client.get(&url).send().await?;
        let status = resp.status();
        let fee_estimations = resp.json::<FeeEstimatesResponse>().await.map_err(|err| {
            tracing::warn!(
                status = %status,
                error = %err,
                "blockstream fee estimation response decode failed"
            );
            FeeEstimationError::CouldNotDecodeResponseBody(err)
        })?;
        match priority {
            TxPriority::HalfHour => Ok(FeeRate::from_sat_per_vb(fee_estimations.half_hour_fee)),
            TxPriority::OneHour => Ok(FeeRate::from_sat_per_vb(fee_estimations.hour_fee)),
            TxPriority::NextBlock => Ok(FeeRate::from_sat_per_vb(fee_estimations.next_block)),
        }
    }
}

#[serde_with::serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockstreamConfig {
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

impl Default for BlockstreamConfig {
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
    "https://blockstream.info".to_string()
}

fn default_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(3)
}

fn default_number_of_retries() -> u32 {
    2
}

fn default_rate_limit_per_second() -> u32 {
    1
}

fn default_rate_limit_burst() -> u32 {
    2
}
