use bdk::FeeRate;

use super::error::FeeEstimationError;
use crate::primitives::TxPriority;

/// Ceiling for any fee rate accepted from an external estimator, in sat/vB.
///
/// Deliberately not configurable: the most extreme organic fee events have
/// peaked around 1_000 sat/vB, so a reading above this is a broken or
/// compromised provider rather than a market condition. Operators narrow the
/// working range with `app.fees.max_sat_per_vb`.
pub const HARD_MAX_SAT_PER_VB: f64 = 10_000.0;

/// Turns a number taken from a provider response into a `FeeRate`.
///
/// This is the only place allowed to call `FeeRate::from_sat_per_vb` on
/// external input: bdk asserts that the value is normal (or zero) and
/// positive, so an unvetted response body can otherwise panic the calling
/// task. Rejecting here - rather than substituting a value - lets the caller
/// fall back to the other provider.
pub fn checked_fee_rate(
    value: f64,
    provider: &'static str,
    priority: TxPriority,
) -> Result<FeeRate, FeeEstimationError> {
    // `value as f32` underflows to zero for tiny values and saturates to
    // infinity for huge ones, so the cast result is what has to be normal.
    let sat_per_vb = value as f32;
    if !value.is_finite() || value <= 0.0 || value > HARD_MAX_SAT_PER_VB || !sat_per_vb.is_normal()
    {
        tracing::warn!(
            provider,
            ?priority,
            value,
            hard_max_sat_per_vb = HARD_MAX_SAT_PER_VB,
            "rejected implausible fee estimate"
        );
        return Err(FeeEstimationError::ImplausibleFeeRate {
            provider,
            priority,
            value,
        });
    }
    Ok(FeeRate::from_sat_per_vb(sat_per_vb))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checked(value: f64) -> Result<FeeRate, FeeEstimationError> {
        checked_fee_rate(value, "test", TxPriority::NextBlock)
    }

    #[test]
    fn accepts_plausible_values() {
        assert_eq!(checked(1.0).unwrap().as_sat_per_vb(), 1.0);
        assert_eq!(checked(42.5).unwrap().as_sat_per_vb(), 42.5);
        assert_eq!(
            checked(HARD_MAX_SAT_PER_VB).unwrap().as_sat_per_vb(),
            HARD_MAX_SAT_PER_VB as f32
        );
    }

    #[test]
    fn rejects_non_positive_values() {
        assert!(checked(0.0).is_err());
        assert!(checked(-1.0).is_err());
    }

    #[test]
    fn rejects_values_above_the_hard_ceiling() {
        assert!(checked(HARD_MAX_SAT_PER_VB + 0.1).is_err());
        assert!(checked(u64::MAX as f64).is_err());
    }

    #[test]
    fn rejects_values_that_cannot_be_represented() {
        assert!(checked(f64::NAN).is_err());
        assert!(checked(f64::INFINITY).is_err());
        assert!(checked(f64::NEG_INFINITY).is_err());
        // underflows to zero when cast to f32
        assert!(checked(1e-300).is_err());
    }
}
