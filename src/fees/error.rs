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
}
