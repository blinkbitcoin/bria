use bdk::FeeRate;

use super::config::{
    default_max_batch_fee_tolerance, default_max_sat_per_vb, default_min_sat_per_vb, FeesConfig,
    OnMaxExceeded,
};
use super::error::{BatchFeeError, FeeEstimationError};
use crate::primitives::TxPriority;

/// Tracing target for the bounds alerts, so that operators have a stable thing
/// to alert on. `action` says what was done: "floor", "clamp" or "reject".
const ALERT_TARGET: &str = "bria::fees::bounds";

/// A batch fee at or below this is too small for the consistency check to say
/// anything useful: bdk folds a change output that would be dust into the fee,
/// which moves a small absolute fee by a large factor.
const BATCH_FEE_GUARD_MIN_SATS: u64 = 25_000;

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

/// A fee rate that has been through [`FeeBounds`].
///
/// The PSBT builder accepts nothing else, so no code path can reach
/// `bdk::TxBuilder::fee_rate` with a rate that skipped the bounds - including
/// future call sites that get a `FeeRate` from somewhere new.
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd)]
pub struct BoundedFeeRate(FeeRate);

impl BoundedFeeRate {
    /// Escape hatch for tests that need to build a PSBT at a chosen rate.
    /// Production code obtains these from `FeesClient`.
    #[doc(hidden)]
    pub fn unchecked(fee_rate: FeeRate) -> Self {
        Self(fee_rate)
    }
}

impl std::ops::Deref for BoundedFeeRate {
    type Target = FeeRate;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// The configured working range for fee rates, applied to every rate handed to
/// a consumer - including one served from the cache, so that a config change
/// takes effect immediately and no cached entry can outlive its bounds.
#[derive(Debug, Clone, Copy)]
pub(super) struct FeeBounds {
    min_sat_per_vb: f64,
    max_sat_per_vb: f64,
    on_max_exceeded: OnMaxExceeded,
    max_batch_fee_tolerance: f64,
}

impl FeeBounds {
    pub(super) fn from_config(config: &FeesConfig) -> Self {
        let min = sane_bound(
            config.min_sat_per_vb,
            default_min_sat_per_vb(),
            "min_sat_per_vb",
        );
        let max = sane_bound(
            config.max_sat_per_vb,
            default_max_sat_per_vb(),
            "max_sat_per_vb",
        );
        // `FeesConfig::validate` rejects this at startup; a library embedder
        // can still build a config by hand, so don't trust it blindly.
        let (min, max) = if min > max {
            tracing::warn!(
                min_sat_per_vb = min,
                max_sat_per_vb = max,
                "min_sat_per_vb is above max_sat_per_vb; falling back to the default fee bounds"
            );
            (default_min_sat_per_vb(), default_max_sat_per_vb())
        } else {
            (min, max)
        };

        let max_batch_fee_tolerance = if !config.max_batch_fee_tolerance.is_finite()
            || config.max_batch_fee_tolerance < 1.0
        {
            tracing::warn!(
                field = "max_batch_fee_tolerance",
                configured = config.max_batch_fee_tolerance,
                default = default_max_batch_fee_tolerance(),
                "invalid fee bound config value; using the default"
            );
            default_max_batch_fee_tolerance()
        } else {
            config.max_batch_fee_tolerance
        };

        Self {
            min_sat_per_vb: min,
            max_sat_per_vb: max,
            on_max_exceeded: config.on_max_exceeded,
            max_batch_fee_tolerance,
        }
    }

    /// Bounds a rate that is about to be used to build a transaction. A reading
    /// above the ceiling is refused by default: see [`OnMaxExceeded::Reject`].
    pub(super) fn enforce(
        &self,
        fee_rate: FeeRate,
        priority: TxPriority,
    ) -> Result<BoundedFeeRate, FeeEstimationError> {
        let observed = f64::from(fee_rate.as_sat_per_vb());
        if observed > self.max_sat_per_vb {
            return match self.on_max_exceeded {
                OnMaxExceeded::Reject => {
                    self.alert_above_max(observed, priority, "reject");
                    Err(FeeEstimationError::FeeRateAboveMaximum {
                        priority,
                        observed,
                        max: self.max_sat_per_vb,
                    })
                }
                OnMaxExceeded::Clamp => {
                    self.alert_above_max(observed, priority, "clamp");
                    Ok(self.ceiling())
                }
            };
        }
        Ok(self.floored(fee_rate, observed, priority))
    }

    /// Bounds a rate used for internal accounting (fee encumbrance), which must
    /// never fail: wallet syncing depends on it and no funds move.
    pub(super) fn clamp(&self, fee_rate: FeeRate, priority: TxPriority) -> BoundedFeeRate {
        let observed = f64::from(fee_rate.as_sat_per_vb());
        if observed > self.max_sat_per_vb {
            self.alert_above_max(observed, priority, "clamp");
            return self.ceiling();
        }
        self.floored(fee_rate, observed, priority)
    }

    /// Last check before a batch is created: the absolute fee has to be
    /// consistent with the rate it was built at and the size it will be. The
    /// bounds above constrain the rate; this constrains the arithmetic that
    /// turned the rate into satoshis, which is where the money actually leaves.
    pub(super) fn check_batch_fee(
        &self,
        fee_sats: u64,
        cpfp_fee_sats: u64,
        vbytes: u64,
        fee_rate: BoundedFeeRate,
    ) -> Result<(), BatchFeeError> {
        // CPFP bump fees are paid on purpose and are not implied by this
        // transaction's own size.
        let own_fee_sats = fee_sats.saturating_sub(cpfp_fee_sats);
        if own_fee_sats <= BATCH_FEE_GUARD_MIN_SATS || vbytes == 0 {
            return Ok(());
        }

        let expected_sats = fee_rate.fee_vb(vbytes as usize);
        let allowed_sats = (expected_sats as f64 * self.max_batch_fee_tolerance).ceil() as u64;
        if own_fee_sats > allowed_sats {
            let sat_per_vb = f64::from(fee_rate.as_sat_per_vb());
            tracing::error!(
                target: ALERT_TARGET,
                observed_sats = own_fee_sats,
                expected_sats,
                allowed_sats,
                vbytes,
                sat_per_vb,
                tolerance = self.max_batch_fee_tolerance,
                action = "reject",
                "batch fee is inconsistent with the fee rate it was built at"
            );
            return Err(BatchFeeError::InconsistentWithFeeRate {
                observed_sats: own_fee_sats,
                expected_sats,
                vbytes,
                sat_per_vb,
                tolerance: self.max_batch_fee_tolerance,
            });
        }
        Ok(())
    }

    fn floored(&self, fee_rate: FeeRate, observed: f64, priority: TxPriority) -> BoundedFeeRate {
        if observed < self.min_sat_per_vb {
            tracing::warn!(
                target: ALERT_TARGET,
                ?priority,
                observed_sat_per_vb = observed,
                min_sat_per_vb = self.min_sat_per_vb,
                action = "floor",
                "fee estimate below configured minimum"
            );
            return BoundedFeeRate(FeeRate::from_sat_per_vb(self.min_sat_per_vb as f32));
        }
        BoundedFeeRate(fee_rate)
    }

    fn ceiling(&self) -> BoundedFeeRate {
        BoundedFeeRate(FeeRate::from_sat_per_vb(self.max_sat_per_vb as f32))
    }

    fn alert_above_max(&self, observed: f64, priority: TxPriority, action: &'static str) {
        tracing::error!(
            target: ALERT_TARGET,
            ?priority,
            observed_sat_per_vb = observed,
            max_sat_per_vb = self.max_sat_per_vb,
            action,
            "fee estimate above configured maximum"
        );
    }
}

fn sane_bound(configured: f64, default: f64, field: &'static str) -> f64 {
    if !configured.is_finite()
        || configured <= 0.0
        || configured > HARD_MAX_SAT_PER_VB
        || !(configured as f32).is_normal()
    {
        tracing::warn!(
            field,
            configured,
            default,
            "invalid fee bound config value; using the default"
        );
        return default;
    }
    configured
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

    fn bounds(min: f64, max: f64, on_max_exceeded: OnMaxExceeded) -> FeeBounds {
        FeeBounds::from_config(&FeesConfig {
            min_sat_per_vb: min,
            max_sat_per_vb: max,
            on_max_exceeded,
            ..Default::default()
        })
    }

    fn rate(sat_per_vb: f32) -> FeeRate {
        FeeRate::from_sat_per_vb(sat_per_vb)
    }

    #[test]
    fn enforce_passes_rates_inside_the_range() {
        let bounds = bounds(1.0, 1_000.0, OnMaxExceeded::Reject);

        assert_eq!(
            *bounds.enforce(rate(42.0), TxPriority::NextBlock).unwrap(),
            rate(42.0)
        );
    }

    #[test]
    fn enforce_rejects_rates_above_the_maximum() {
        let bounds = bounds(1.0, 1_000.0, OnMaxExceeded::Reject);

        let err = bounds
            .enforce(rate(1_000.5), TxPriority::NextBlock)
            .unwrap_err();

        assert!(matches!(
            err,
            FeeEstimationError::FeeRateAboveMaximum { max, .. } if max == 1_000.0
        ));
    }

    #[test]
    fn enforce_clamps_when_the_operator_asked_for_it() {
        let bounds = bounds(1.0, 1_000.0, OnMaxExceeded::Clamp);

        assert_eq!(
            *bounds
                .enforce(rate(5_000.0), TxPriority::NextBlock)
                .unwrap(),
            rate(1_000.0)
        );
    }

    #[test]
    fn enforce_raises_rates_below_the_minimum() {
        let bounds = bounds(5.0, 1_000.0, OnMaxExceeded::Reject);

        assert_eq!(
            *bounds.enforce(rate(2.0), TxPriority::NextBlock).unwrap(),
            rate(5.0)
        );
    }

    #[test]
    fn clamp_never_fails() {
        let bounds = bounds(5.0, 1_000.0, OnMaxExceeded::Reject);

        assert_eq!(
            *bounds.clamp(rate(9_999.0), TxPriority::NextBlock),
            rate(1_000.0)
        );
        assert_eq!(*bounds.clamp(rate(1.0), TxPriority::NextBlock), rate(5.0));
        assert_eq!(*bounds.clamp(rate(42.0), TxPriority::NextBlock), rate(42.0));
    }

    #[test]
    fn nonsensical_config_values_fall_back_to_defaults() {
        let inverted = bounds(900.0, 10.0, OnMaxExceeded::Reject);
        assert_eq!(
            *inverted.clamp(rate(500.0), TxPriority::NextBlock),
            rate(500.0)
        );

        let out_of_range = bounds(0.0, HARD_MAX_SAT_PER_VB * 10.0, OnMaxExceeded::Reject);
        assert_eq!(
            *out_of_range.clamp(rate(2_000.0), TxPriority::NextBlock),
            rate(1_000.0)
        );
    }

    #[test]
    fn batch_fee_consistent_with_the_rate_is_accepted() {
        let bounds = bounds(1.0, 1_000.0, OnMaxExceeded::Reject);
        let fee_rate = BoundedFeeRate::unchecked(rate(20.0));
        // 20 sat/vB over 5_000 vbytes
        assert!(bounds.check_batch_fee(100_000, 0, 5_000, fee_rate).is_ok());
    }

    #[test]
    fn batch_fee_far_above_the_rate_is_refused() {
        let bounds = bounds(1.0, 1_000.0, OnMaxExceeded::Reject);
        let fee_rate = BoundedFeeRate::unchecked(rate(20.0));

        let err = bounds
            .check_batch_fee(10_000_000, 0, 5_000, fee_rate)
            .unwrap_err();

        assert!(matches!(
            err,
            BatchFeeError::InconsistentWithFeeRate { expected_sats, .. } if expected_sats == 100_000
        ));
    }

    #[test]
    fn batch_fee_within_the_tolerance_is_accepted() {
        let bounds = bounds(1.0, 1_000.0, OnMaxExceeded::Reject);
        let fee_rate = BoundedFeeRate::unchecked(rate(20.0));

        // 2x the implied fee is inside the default 3x tolerance
        assert!(bounds.check_batch_fee(200_000, 0, 5_000, fee_rate).is_ok());
        assert!(bounds.check_batch_fee(400_000, 0, 5_000, fee_rate).is_err());
    }

    #[test]
    fn small_batch_fees_are_left_alone() {
        let bounds = bounds(1.0, 1_000.0, OnMaxExceeded::Reject);
        let fee_rate = BoundedFeeRate::unchecked(rate(1.0));

        // dust folded into the fee can move a small fee by a large factor
        assert!(bounds.check_batch_fee(25_000, 0, 250, fee_rate).is_ok());
    }

    #[test]
    fn cpfp_bump_fees_do_not_count_against_the_batch() {
        let bounds = bounds(1.0, 1_000.0, OnMaxExceeded::Reject);
        let fee_rate = BoundedFeeRate::unchecked(rate(20.0));

        // the batch's own fee is 100_000; the rest is a deliberate bump
        assert!(bounds
            .check_batch_fee(10_000_000, 9_900_000, 5_000, fee_rate)
            .is_ok());
    }

    #[test]
    fn config_validation_catches_unusable_bounds() {
        let valid = FeesConfig::default();
        assert!(valid.validate().is_ok());

        let bad_tolerance = FeesConfig {
            max_batch_fee_tolerance: 0.5,
            ..Default::default()
        };
        assert!(bad_tolerance.validate().is_err());

        for (min, max) in [
            (0.0, 1_000.0),
            (-1.0, 1_000.0),
            (f64::NAN, 1_000.0),
            (1.0, 0.0),
            (1.0, f64::INFINITY),
            (900.0, 10.0),
            (1.0, HARD_MAX_SAT_PER_VB + 1.0),
        ] {
            let config = FeesConfig {
                min_sat_per_vb: min,
                max_sat_per_vb: max,
                ..Default::default()
            };
            assert!(
                config.validate().is_err(),
                "expected min={min} max={max} to be rejected"
            );
        }
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
