use serde::{Deserialize, Serialize};

use super::blockstream::BlockstreamConfig;
use super::bounds::HARD_MAX_SAT_PER_VB;
use super::error::FeeBoundsConfigError;
use super::mempool_space::MempoolSpaceConfig;

#[serde_with::serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeesConfig {
    #[serde(default)]
    pub mempool_space: MempoolSpaceConfig,
    #[serde(default)]
    pub blockstream: BlockstreamConfig,
    #[serde_as(as = "serde_with::DurationSeconds<u64>")]
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl: std::time::Duration,
    #[serde_as(as = "serde_with::DurationSeconds<u64>")]
    #[serde(default = "default_stale_ttl")]
    pub stale_ttl: std::time::Duration,
    #[serde(default = "default_enable_stale_on_error")]
    pub enable_stale_on_error: bool,
    /// Readings below this are raised to it.
    #[serde(default = "default_min_sat_per_vb")]
    pub min_sat_per_vb: f64,
    /// Readings above this are refused (see `on_max_exceeded`) when spending, clamped for accounting.
    #[serde(default = "default_max_sat_per_vb")]
    pub max_sat_per_vb: f64,
    #[serde(default)]
    pub on_max_exceeded: OnMaxExceeded,
    /// Max ratio of a batch's fee to rate x size before the batch is refused.
    #[serde(default = "default_max_batch_fee_tolerance")]
    pub max_batch_fee_tolerance: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnMaxExceeded {
    /// Skip batching until the next tick. Default, because a signed batch can't be cancelled.
    #[default]
    Reject,
    /// Use `max_sat_per_vb` instead.
    Clamp,
}

impl FeesConfig {
    pub fn validate(&self) -> Result<(), FeeBoundsConfigError> {
        if !self.min_sat_per_vb.is_finite() || self.min_sat_per_vb <= 0.0 {
            return Err(FeeBoundsConfigError::InvalidMin(self.min_sat_per_vb));
        }
        if !self.max_sat_per_vb.is_finite() || self.max_sat_per_vb <= 0.0 {
            return Err(FeeBoundsConfigError::InvalidMax(self.max_sat_per_vb));
        }
        if self.min_sat_per_vb > self.max_sat_per_vb {
            return Err(FeeBoundsConfigError::MinAboveMax {
                min: self.min_sat_per_vb,
                max: self.max_sat_per_vb,
            });
        }
        if self.max_sat_per_vb > HARD_MAX_SAT_PER_VB {
            return Err(FeeBoundsConfigError::MaxAboveHardCeiling {
                max: self.max_sat_per_vb,
                hard_max: HARD_MAX_SAT_PER_VB,
            });
        }
        if !self.max_batch_fee_tolerance.is_finite() || self.max_batch_fee_tolerance < 1.0 {
            return Err(FeeBoundsConfigError::InvalidBatchFeeTolerance(
                self.max_batch_fee_tolerance,
            ));
        }
        Ok(())
    }
}

impl Default for FeesConfig {
    fn default() -> Self {
        Self {
            mempool_space: MempoolSpaceConfig::default(),
            blockstream: BlockstreamConfig::default(),
            cache_ttl: default_cache_ttl(),
            stale_ttl: default_stale_ttl(),
            enable_stale_on_error: default_enable_stale_on_error(),
            min_sat_per_vb: default_min_sat_per_vb(),
            max_sat_per_vb: default_max_sat_per_vb(),
            on_max_exceeded: OnMaxExceeded::default(),
            max_batch_fee_tolerance: default_max_batch_fee_tolerance(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct FeeCacheConfig {
    pub cache_ttl: std::time::Duration,
    pub stale_ttl: std::time::Duration,
    pub enable_stale_on_error: bool,
}

impl From<&FeesConfig> for FeeCacheConfig {
    fn from(config: &FeesConfig) -> Self {
        Self {
            cache_ttl: config.cache_ttl,
            stale_ttl: config.stale_ttl,
            enable_stale_on_error: config.enable_stale_on_error,
        }
    }
}

fn default_cache_ttl() -> std::time::Duration {
    std::time::Duration::from_secs(15)
}

fn default_stale_ttl() -> std::time::Duration {
    std::time::Duration::from_secs(120)
}

fn default_enable_stale_on_error() -> bool {
    true
}

pub(super) fn default_min_sat_per_vb() -> f64 {
    1.0
}

pub(super) fn default_max_sat_per_vb() -> f64 {
    1_000.0
}

pub(super) fn default_max_batch_fee_tolerance() -> f64 {
    3.0
}
