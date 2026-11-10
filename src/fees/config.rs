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
    /// Fee rates below this are raised to it: a provider reporting less than
    /// the node's min relay fee would otherwise build transactions that never
    /// confirm.
    #[serde(default = "default_min_sat_per_vb")]
    pub min_sat_per_vb: f64,
    /// Fee rates above this are refused (see `on_max_exceeded`) on the paths
    /// that spend, and clamped on the paths that only estimate.
    #[serde(default = "default_max_sat_per_vb")]
    pub max_sat_per_vb: f64,
    #[serde(default)]
    pub on_max_exceeded: OnMaxExceeded,
    /// How far a finished batch's absolute fee may exceed what its fee rate and
    /// size imply before the batch is refused. Catches a mistake in the fee
    /// arithmetic itself, which bounding the rate cannot.
    #[serde(default = "default_max_batch_fee_tolerance")]
    pub max_batch_fee_tolerance: f64,
}

/// What to do when an estimator reports more than `max_sat_per_vb` on a path
/// that is about to spend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnMaxExceeded {
    /// Refuse to use the rate. The payout queue job errors out and the batch is
    /// retried on the next tick, so no transaction is built while the reading
    /// stands. This is the default: a batch built at a clamped-down rate is
    /// signed and broadcast within seconds, cannot be cancelled afterwards, and
    /// holds its inputs reserved until it confirms.
    #[default]
    Reject,
    /// Use `max_sat_per_vb` instead. Favours liveness over fee discipline.
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

/// Well above every sustained organic fee regime to date (the most extreme
/// events peaked around 1_000 sat/vB and only briefly), so the default only
/// fires on readings that are not credible market data. Operators who would
/// rather hold payouts than pay a spike should lower this.
pub(super) fn default_max_sat_per_vb() -> f64 {
    1_000.0
}

/// Generous on purpose: the batch guard is there to catch a fee that is wrong
/// by an order of magnitude, not to second-guess the builder's arithmetic.
pub(super) fn default_max_batch_fee_tolerance() -> f64 {
    3.0
}
