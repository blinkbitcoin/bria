use thiserror::Error;

use crate::primitives::TxPriority;

#[derive(Debug, Error)]
pub enum FeeEstimationError {
    #[error("FeeEstimationError - FeeEstimation: {0}")]
    FeeEstimation(#[from] reqwest_middleware::Error),
    #[error("FeeEstimationError - CouldNotDecodeResponseBody: {0}")]
    CouldNotDecodeResponseBody(reqwest::Error),
    #[error(
        "FeeEstimationError - ImplausibleFeeRate: {provider} returned {value} sat/vB for {priority:?}"
    )]
    ImplausibleFeeRate {
        provider: &'static str,
        priority: TxPriority,
        value: f64,
    },
    #[error(
        "FeeEstimationError - FeeRateAboveMaximum: estimate of {observed} sat/vB for {priority:?} exceeds the configured max_sat_per_vb of {max}"
    )]
    FeeRateAboveMaximum {
        priority: TxPriority,
        observed: f64,
        max: f64,
    },
}

#[derive(Debug, Error)]
pub enum FeeBoundsConfigError {
    #[error("app.fees.min_sat_per_vb must be a finite value greater than 0, got {0}")]
    InvalidMin(f64),
    #[error("app.fees.max_sat_per_vb must be a finite value greater than 0, got {0}")]
    InvalidMax(f64),
    #[error(
        "app.fees.min_sat_per_vb ({min}) must not be greater than app.fees.max_sat_per_vb ({max})"
    )]
    MinAboveMax { min: f64, max: f64 },
    #[error(
        "app.fees.max_sat_per_vb ({max}) must not exceed the hard ceiling of {hard_max} sat/vB"
    )]
    MaxAboveHardCeiling { max: f64, hard_max: f64 },
    #[error("app.fees.max_batch_fee_tolerance must be a finite value of at least 1.0, got {0}")]
    InvalidBatchFeeTolerance(f64),
}

#[derive(Debug, Error)]
pub enum BatchFeeError {
    #[error(
        "BatchFeeError - InconsistentWithFeeRate: batch fee of {observed_sats} sats is more than {tolerance}x the {expected_sats} sats implied by {sat_per_vb} sat/vB over {vbytes} vbytes"
    )]
    InconsistentWithFeeRate {
        observed_sats: u64,
        expected_sats: u64,
        vbytes: u64,
        sat_per_vb: f64,
        tolerance: f64,
    },
}
