use bdk::FeeRate;
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::sync::Mutex;
use tracing::instrument;

use super::{blockstream::*, bounds::*, config::*, error::*, mempool_space::*};
use crate::primitives::TxPriority;

#[derive(Clone, Debug)]
struct CachedFeeRate {
    fee_rate: FeeRate,
    fetched_at: Instant,
    source: &'static str,
}

#[derive(Clone, Debug)]
struct PriorityLocks {
    next_block: Arc<Mutex<()>>,
    half_hour: Arc<Mutex<()>>,
    one_hour: Arc<Mutex<()>>,
}

impl PriorityLocks {
    fn new() -> Self {
        Self {
            next_block: Arc::new(Mutex::new(())),
            half_hour: Arc::new(Mutex::new(())),
            one_hour: Arc::new(Mutex::new(())),
        }
    }

    fn for_priority(&self, priority: TxPriority) -> Arc<Mutex<()>> {
        match priority {
            TxPriority::NextBlock => self.next_block.clone(),
            TxPriority::HalfHour => self.half_hour.clone(),
            TxPriority::OneHour => self.one_hour.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FeesClient {
    mempool_space: MempoolSpaceClient,
    blockstream: BlockstreamClient,
    cache: Arc<Mutex<HashMap<TxPriority, CachedFeeRate>>>,
    locks: PriorityLocks,
    cache_config: FeeCacheConfig,
    bounds: FeeBounds,
}

fn record_fee_rate(fee_rate: BoundedFeeRate) {
    tracing::Span::current().record("fee_rate", tracing::field::debug(*fee_rate));
}

impl FeesClient {
    pub fn new(config: FeesConfig) -> Self {
        let cache_config = FeeCacheConfig::from(&config);
        let bounds = FeeBounds::from_config(&config);
        Self {
            mempool_space: MempoolSpaceClient::new(config.mempool_space),
            blockstream: BlockstreamClient::new(config.blockstream),
            cache: Arc::new(Mutex::new(HashMap::new())),
            locks: PriorityLocks::new(),
            cache_config,
            bounds,
        }
    }

    /// Fee rate for building a transaction. Bounded by the configured range;
    /// an estimate above `max_sat_per_vb` is refused rather than silently
    /// turned into a transaction that pays it.
    #[instrument(name = "fees.fee_rate", skip(self), fields(fee_rate), err)]
    pub async fn fee_rate(
        &self,
        priority: TxPriority,
    ) -> Result<BoundedFeeRate, FeeEstimationError> {
        let fee_rate = self
            .bounds
            .enforce(self.estimated_fee_rate(priority).await?, priority)?;
        record_fee_rate(fee_rate);
        Ok(fee_rate)
    }

    /// Fee rate for internal accounting, such as the spending fees encumbered
    /// during a wallet sync. Always clamped into the configured range: no funds
    /// move, and syncing must not stop because the estimators are off.
    #[instrument(
        name = "fees.fee_rate_for_accounting",
        skip(self),
        fields(fee_rate),
        err
    )]
    pub async fn fee_rate_for_accounting(
        &self,
        priority: TxPriority,
    ) -> Result<BoundedFeeRate, FeeEstimationError> {
        let fee_rate = self
            .bounds
            .clamp(self.estimated_fee_rate(priority).await?, priority);
        record_fee_rate(fee_rate);
        Ok(fee_rate)
    }

    /// The raw estimate from the cache or the providers, before any bounds.
    async fn estimated_fee_rate(
        &self,
        priority: TxPriority,
    ) -> Result<FeeRate, FeeEstimationError> {
        if let Some(cached) = self.try_cache_hit(priority).await {
            tracing::debug!(source = cached.source, "fee cache hit");
            return Ok(cached.fee_rate);
        }

        let lock = self.locks.for_priority(priority);
        let _guard = lock.lock().await;

        if let Some(cached) = self.try_cache_hit(priority).await {
            tracing::debug!(source = cached.source, "fee cache hit after wait");
            return Ok(cached.fee_rate);
        }

        let (fetched_fee_rate, fetched_source) = match self.mempool_space.fee_rate(priority).await {
            Ok(fee_rate) => (fee_rate, "mempool_space"),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "mempool_space fee estimation failed, falling back to blockstream"
                );
                match self.blockstream.fee_rate(priority).await {
                    Ok(fee_rate) => (fee_rate, "blockstream"),
                    Err(blockstream_err) => {
                        if self.cache_config.enable_stale_on_error {
                            if let Some(stale) = self.try_stale_cache_hit(priority).await {
                                let stale_age_seconds = stale.fetched_at.elapsed().as_secs_f64();
                                tracing::warn!(
                                    mempool_space_error = %err,
                                    blockstream_error = %blockstream_err,
                                    source = stale.source,
                                    stale_age_seconds,
                                    "fee providers failed, returning stale cached fee"
                                );
                                return Ok(stale.fee_rate);
                            }
                        }
                        return Err(blockstream_err);
                    }
                }
            }
        };

        self.cache.lock().await.insert(
            priority,
            CachedFeeRate {
                fee_rate: fetched_fee_rate,
                fetched_at: Instant::now(),
                source: fetched_source,
            },
        );

        Ok(fetched_fee_rate)
    }

    async fn try_cache_hit(&self, priority: TxPriority) -> Option<CachedFeeRate> {
        let cache = self.cache.lock().await;
        cache
            .get(&priority)
            .filter(|entry| entry.fetched_at.elapsed() <= self.cache_config.cache_ttl)
            .cloned()
    }

    async fn try_stale_cache_hit(&self, priority: TxPriority) -> Option<CachedFeeRate> {
        let cache = self.cache.lock().await;
        cache
            .get(&priority)
            .filter(|entry| entry.fetched_at.elapsed() <= self.cache_config.stale_ttl)
            .cloned()
    }
}
