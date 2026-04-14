use bdk::{
    blockchain::{GetHeight, Progress, WalletSync},
    database::{BatchDatabase, Database},
    wallet::{signer::SignOptions, AddressIndex, SyncOptions},
    Wallet,
};
use sqlx::PgPool;
use std::{sync::Mutex, time::Duration};
use tracing::instrument;
use uuid::Uuid;

use super::config::*;
use crate::{
    bdk::{error::BdkError, pg::SqlxWalletDb},
    primitives::{bitcoin::*, *},
};

pub trait BdkWalletVisitor: Sized + Send + 'static {
    fn visit_bdk_wallet<D: BatchDatabase>(
        self,
        keychain_id: KeychainId,
        wallet: &Wallet<D>,
    ) -> Result<Self, BdkError>;
}

pub struct KeychainWallet {
    pub keychain_id: KeychainId,
    pool: PgPool,
    network: Network,
    config: KeychainConfig,
}

#[derive(Debug, Clone)]
pub struct SyncProgressContext {
    pub wallet_id: WalletId,
    pub keychain_id: KeychainId,
    pub sync_run_id: String,
}

impl SyncProgressContext {
    pub fn new(wallet_id: WalletId, keychain_id: KeychainId) -> Self {
        Self {
            wallet_id,
            keychain_id,
            sync_run_id: Uuid::new_v4().to_string(),
        }
    }

    pub fn with_sync_run_id(
        wallet_id: WalletId,
        keychain_id: KeychainId,
        sync_run_id: String,
    ) -> Self {
        Self {
            wallet_id,
            keychain_id,
            sync_run_id,
        }
    }
}

const PROGRESS_BUCKET_SIZE_PCT: u8 = 10;
const PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

const fn completion_bucket() -> u8 {
    100 / PROGRESS_BUCKET_SIZE_PCT
}

#[derive(Debug)]
struct ProgressState {
    last_bucket: Option<u8>,
    last_emit_at: std::time::Instant,
}

#[derive(Debug)]
struct TracingBdkProgress {
    context: SyncProgressContext,
    state: Mutex<ProgressState>,
}

impl TracingBdkProgress {
    fn new(context: SyncProgressContext) -> Self {
        Self {
            context,
            state: Mutex::new(ProgressState {
                last_bucket: None,
                last_emit_at: std::time::Instant::now(),
            }),
        }
    }
}

impl Progress for TracingBdkProgress {
    fn update(&self, progress: f32, message: Option<String>) -> Result<(), bdk::Error> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                tracing::warn!(
                    sync_run_id = %self.context.sync_run_id,
                    wallet_id = %self.context.wallet_id,
                    keychain_id = %self.context.keychain_id,
                    "bdk progress state mutex poisoned; recovering"
                );
                poisoned.into_inner()
            }
        };
        let elapsed = state.last_emit_at.elapsed();

        if !should_emit_progress(state.last_bucket, progress, elapsed) {
            return Ok(());
        }

        let bucket = progress_bucket(progress);
        state.last_bucket = Some(bucket);
        state.last_emit_at = std::time::Instant::now();

        tracing::info!(
            sync_run_id = %self.context.sync_run_id,
            wallet_id = %self.context.wallet_id,
            keychain_id = %self.context.keychain_id,
            progress_pct = progress,
            progress_message = message.as_deref().unwrap_or(""),
            "wallet sync progress"
        );

        Ok(())
    }
}

fn progress_bucket(progress: f32) -> u8 {
    (progress.clamp(0.0, 100.0) as u8 / PROGRESS_BUCKET_SIZE_PCT).min(completion_bucket())
}

fn should_emit_progress(
    last_bucket: Option<u8>,
    progress: f32,
    elapsed_since_last_emit: Duration,
) -> bool {
    if last_bucket.is_none() {
        return true;
    }

    let bucket = progress_bucket(progress);
    if last_bucket != Some(bucket) {
        return true;
    }

    if progress >= 100.0 {
        return last_bucket != Some(completion_bucket());
    }

    elapsed_since_last_emit >= PROGRESS_HEARTBEAT_INTERVAL
}

impl KeychainWallet {
    pub fn new(
        pool: PgPool,
        network: Network,
        keychain_id: KeychainId,
        descriptors: KeychainConfig,
    ) -> Self {
        Self {
            pool,
            network,
            keychain_id,
            config: descriptors,
        }
    }

    pub async fn finalize_psbt(
        &self,
        mut psbt: psbt::PartiallySignedTransaction,
    ) -> Result<Option<psbt::PartiallySignedTransaction>, BdkError> {
        match self
            .with_wallet(move |wallet| {
                if wallet.finalize_psbt(&mut psbt, SignOptions::default())? {
                    Ok::<_, BdkError>(Some(psbt))
                } else {
                    Ok::<_, BdkError>(None)
                }
            })
            .await
        {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.into()),
        }
    }

    #[instrument(name = "keychain_wallet.new_external_address", skip_all)]
    pub async fn new_external_address(&self) -> Result<bdk::wallet::AddressInfo, BdkError> {
        let addr = self
            .with_wallet(|wallet| wallet.get_address(AddressIndex::New))
            .await??;
        Ok(addr)
    }

    #[instrument(name = "keychain_wallet.example_address", skip_all)]
    pub async fn example_address(&self) -> Result<bdk::wallet::AddressInfo, BdkError> {
        let addr = self
            .with_wallet(|wallet| wallet.get_address(AddressIndex::Peek(0)))
            .await??;
        Ok(addr)
    }

    #[instrument(name = "keychain_wallet.new_internal_address", skip_all)]
    pub async fn new_internal_address(&self) -> Result<bdk::wallet::AddressInfo, BdkError> {
        let addr = self
            .with_wallet(|wallet| wallet.get_internal_address(AddressIndex::New))
            .await??;
        Ok(addr)
    }

    pub async fn find_address_from_path(
        &self,
        path: u32,
        kind: KeychainKind,
    ) -> Result<bdk::wallet::AddressInfo, BdkError> {
        let addr = self
            .with_wallet(move |wallet| match kind {
                KeychainKind::External => wallet.get_address(AddressIndex::Peek(path)),
                KeychainKind::Internal => wallet.get_internal_address(AddressIndex::Peek(path)),
            })
            .await??;
        Ok(addr)
    }

    #[instrument(name = "keychain_wallet.sync", skip_all)]
    pub async fn sync<B: WalletSync + GetHeight + Send + Sync + 'static>(
        &self,
        blockchain: B,
        context: SyncProgressContext,
    ) -> Result<(), BdkError> {
        let sync_span = tracing::Span::current();
        self.with_wallet(move |wallet| {
            let _span_guard = sync_span.enter();
            let last_external = wallet
                .database()
                .get_last_index(KeychainKind::External)?
                .unwrap_or(0);
            let last_internal = wallet
                .database()
                .get_last_index(KeychainKind::Internal)?
                .unwrap_or(0);
            let max_last_index = last_external.max(last_internal);

            let _ = wallet.ensure_addresses_cached(max_last_index.saturating_add(1))?;
            let progress = TracingBdkProgress::new(context);
            wallet.sync(
                &blockchain,
                SyncOptions {
                    progress: Some(Box::new(progress)),
                },
            )
        })
        .await??;
        Ok(())
    }

    #[instrument(name = "keychain_wallet.balance", skip_all)]
    pub async fn balance(&self) -> Result<bdk::Balance, BdkError> {
        let balance = self.with_wallet(|wallet| wallet.get_balance()).await??;
        Ok(balance)
    }

    #[instrument(name = "keychain_wallet.max_satisfaction_weight", skip_all)]
    pub fn max_satisfaction_weight(&self) -> usize {
        self.config
            .external_descriptor()
            .max_satisfaction_weight()
            .expect("max_satisfaction_weight")
    }

    async fn with_wallet<F, R>(&self, f: F) -> Result<R, tokio::task::JoinError>
    where
        F: 'static + Send + FnOnce(Wallet<SqlxWalletDb>) -> R,
        R: Send + 'static,
    {
        let external = self.config.external_descriptor();
        let internal = self.config.internal_descriptor();
        let pool = self.pool.clone();
        let keychain_id = self.keychain_id;
        let network = self.network;
        let res = tokio::task::spawn_blocking(move || {
            let wallet = Wallet::new(
                external,
                Some(internal),
                network,
                SqlxWalletDb::new(pool, keychain_id),
            )
            .expect("Couldn't construct wallet");
            f(wallet)
        })
        .await?;
        Ok(res)
    }

    pub async fn dispatch_bdk_wallet<V: BdkWalletVisitor>(&self, v: V) -> Result<V, BdkError> {
        let keychain_id = self.keychain_id;
        match self
            .with_wallet(move |wallet| v.visit_bdk_wallet(keychain_id, &wallet))
            .await
        {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_first_progress_update() {
        assert!(should_emit_progress(None, 1.0, Duration::from_secs(1)));
    }

    #[test]
    fn emits_when_bucket_changes() {
        let elapsed = Duration::from_secs(1);
        assert!(should_emit_progress(Some(0), 11.0, elapsed));
        assert!(!should_emit_progress(Some(1), 15.0, elapsed));
    }

    #[test]
    fn emits_on_heartbeat_interval() {
        assert!(should_emit_progress(
            Some(3),
            35.0,
            PROGRESS_HEARTBEAT_INTERVAL
        ));
    }

    #[test]
    fn emits_at_completion_even_without_bucket_change() {
        assert!(should_emit_progress(Some(9), 100.0, Duration::from_secs(1)));
    }

    #[test]
    fn does_not_repeat_completion_event() {
        assert!(!should_emit_progress(
            Some(10),
            100.0,
            Duration::from_secs(1)
        ));
    }
}
