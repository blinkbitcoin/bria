mod cache;
mod convert;
mod db_traits;
mod descriptor_checksum;
mod index;
mod lookups;
mod script_pubkeys;
mod sync_times;
mod transactions;
mod utxos;

use bdk::{
    bitcoin::{ScriptBuf, Txid},
    KeychainKind, LocalUtxo, TransactionDetails,
};
use sqlx::PgPool;
use tokio::runtime::Handle;

use crate::primitives::*;
use cache::WalletCache;
use descriptor_checksum::DescriptorChecksums;
use index::Indexes;
use lookups::MissResolutionPolicy;
use script_pubkeys::ScriptPubkeys;
use std::collections::HashMap;
pub(super) use sync_times::SyncTimes;
pub use transactions::*;
pub use utxos::*;

type ScriptPubkeyCache = HashMap<ScriptBuf, (KeychainKind, u32)>;
type TransactionCache = HashMap<Txid, TransactionDetails>;

#[derive(Clone)]
struct WalletDbContext {
    rt: Handle,
    pool: PgPool,
    keychain_id: KeychainId,
}

impl WalletDbContext {
    fn new(pool: PgPool, keychain_id: KeychainId) -> Self {
        Self {
            rt: Handle::current(),
            pool,
            keychain_id,
        }
    }
}

#[derive(Default)]
struct WalletBatchState {
    utxos: Vec<LocalUtxo>,
    addresses: ScriptPubkeyCache,
    txs: TransactionCache,
}

pub struct SqlxWalletDb {
    ctx: WalletDbContext,
    cache: WalletCache,
    batch: WalletBatchState,
    miss_resolution: MissResolutionPolicy,
}

impl SqlxWalletDb {
    fn unsupported_operation(operation: &str) -> bdk::Error {
        bdk::Error::Generic(format!("{operation} is not supported by SqlxWalletDb"))
    }

    pub fn new(pool: PgPool, keychain_id: KeychainId) -> Self {
        Self {
            ctx: WalletDbContext::new(pool, keychain_id),
            cache: WalletCache::new(),
            batch: WalletBatchState::default(),
            miss_resolution: MissResolutionPolicy::default(),
        }
    }

    pub fn tx_count(&self) -> Result<i64, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.transactions_repo().count_active().await })
    }

    pub fn prewarm_raw_txs(&self) -> Result<usize, bdk::Error> {
        use bdk::database::Database;

        Ok(Database::iter_txs(self, true)?.len())
    }

    fn script_pubkeys_repo(&self) -> ScriptPubkeys {
        ScriptPubkeys::new(self.ctx.keychain_id, self.ctx.pool.clone())
    }

    fn cache_loaded_script_pubkeys(
        cache: &WalletCache,
        keychain: Option<KeychainKind>,
        scripts_with_paths: Vec<(ScriptBuf, (KeychainKind, u32))>,
    ) -> Result<Vec<ScriptBuf>, bdk::Error> {
        cache.extend_script_pubkeys(
            scripts_with_paths
                .iter()
                .map(|(script, path)| (script.clone(), *path)),
        )?;
        cache.mark_script_pubkeys_loaded(keychain);

        Ok(scripts_with_paths
            .into_iter()
            .map(|(script, _)| script)
            .collect())
    }

    fn utxos_repo(&self) -> Utxos {
        Utxos::new(self.ctx.keychain_id, self.ctx.pool.clone())
    }

    fn transactions_repo(&self) -> Transactions {
        Transactions::new(self.ctx.keychain_id, self.ctx.pool.clone())
    }

    fn indexes_repo(&self) -> Indexes {
        Indexes::new(self.ctx.keychain_id, self.ctx.pool.clone())
    }

    fn sync_times_repo(&self) -> SyncTimes {
        SyncTimes::new(self.ctx.keychain_id, self.ctx.pool.clone())
    }

    fn descriptor_checksums_repo(&self) -> DescriptorChecksums {
        DescriptorChecksums::new(self.ctx.keychain_id, self.ctx.pool.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk::bitcoin::hashes::Hash;

    fn tx_details(txid: Txid) -> TransactionDetails {
        TransactionDetails {
            transaction: None,
            txid,
            received: 0,
            sent: 0,
            fee: None,
            confirmation_time: None,
        }
    }

    #[test]
    fn wallet_cache_can_insert_get_and_remove_transactions() {
        let cache = WalletCache::new();
        let txid = Txid::all_zeros();
        let details = tx_details(txid);

        cache
            .extend_txs([(txid, details.clone())])
            .expect("insert should succeed");
        let loaded = cache.get_tx(&txid).expect("get should succeed");
        assert_eq!(loaded, Some(details));

        cache.remove_tx(&txid).expect("remove should succeed");
        let loaded = cache.get_tx(&txid).expect("get should succeed");
        assert_eq!(loaded, None);
    }

    #[test]
    fn wallet_cache_can_insert_and_get_script_pubkey_paths() {
        let cache = WalletCache::new();
        let script = ScriptBuf::new();
        let path = (KeychainKind::External, 42);

        cache
            .insert_script_pubkey(script.clone(), path)
            .expect("insert should succeed");

        let loaded = cache
            .get_script_pubkey_path(script.as_script())
            .expect("get should succeed");
        assert_eq!(loaded, Some(path));
    }

    #[test]
    fn wallet_cache_raw_txs_loaded_flag_defaults_false_and_can_be_set() {
        let cache = WalletCache::new();
        assert!(!cache.raw_txs_fully_loaded());
        assert!(!cache.summary_txs_fully_loaded());

        cache.set_raw_txs_fully_loaded();
        assert!(cache.raw_txs_fully_loaded());
        assert!(cache.summary_txs_fully_loaded());
    }

    #[test]
    fn wallet_cache_summary_txs_loaded_flag_defaults_false_and_can_be_set() {
        let cache = WalletCache::new();
        assert!(!cache.summary_txs_fully_loaded());

        cache.set_summary_txs_fully_loaded();
        assert!(cache.summary_txs_fully_loaded());
    }

    #[test]
    fn wallet_cache_script_pubkeys_loaded_flags_track_keychains() {
        let cache = WalletCache::new();

        assert!(!cache.script_pubkeys_fully_loaded(Some(KeychainKind::External)));
        assert!(!cache.script_pubkeys_fully_loaded(Some(KeychainKind::Internal)));
        assert!(!cache.script_pubkeys_fully_loaded(None));

        cache.mark_script_pubkeys_loaded(Some(KeychainKind::External));
        assert!(cache.script_pubkeys_fully_loaded(Some(KeychainKind::External)));
        assert!(!cache.script_pubkeys_fully_loaded(Some(KeychainKind::Internal)));
        assert!(!cache.script_pubkeys_fully_loaded(None));

        cache.mark_script_pubkeys_loaded(Some(KeychainKind::Internal));
        assert!(cache.script_pubkeys_fully_loaded(None));
    }

    #[test]
    fn overlay_batch_txs_strips_raw_when_include_raw_is_false() {
        let txid = Txid::all_zeros();
        let mut base = HashMap::new();
        base.insert(txid, tx_details(txid));

        let raw_tx = bdk::bitcoin::Transaction {
            version: 2,
            lock_time: bdk::bitcoin::absolute::LockTime::ZERO,
            input: Vec::new(),
            output: Vec::new(),
        };

        let mut batch = HashMap::new();
        let mut batch_tx = tx_details(txid);
        batch_tx.transaction = Some(raw_tx);
        batch.insert(txid, batch_tx);

        let merged = SqlxWalletDb::overlay_batch_txs(base, &batch, false);
        assert!(merged
            .get(&txid)
            .expect("merged tx should exist")
            .transaction
            .is_none());
    }

    #[test]
    fn wallet_cache_all_txs_returns_cached_values() {
        let cache = WalletCache::new();
        let txid = Txid::all_zeros();

        cache
            .extend_txs([(txid, tx_details(txid))])
            .expect("insert should succeed");

        let txs = cache.all_txs().expect("all_txs should succeed");
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].txid, txid);
    }

    #[test]
    fn wallet_cache_extend_summary_txs_preserves_raw_and_refreshes_metadata() {
        let cache = WalletCache::new();
        let txid = Txid::all_zeros();

        let raw_tx = bdk::bitcoin::Transaction {
            version: 2,
            lock_time: bdk::bitcoin::absolute::LockTime::ZERO,
            input: Vec::new(),
            output: Vec::new(),
        };

        let mut existing = tx_details(txid);
        existing.transaction = Some(raw_tx.clone());
        existing.received = 1;
        cache
            .extend_txs([(txid, existing)])
            .expect("insert should succeed");

        let mut summary = tx_details(txid);
        summary.received = 42;
        summary.sent = 7;
        summary.fee = Some(3);
        summary.confirmation_time = Some(bdk::BlockTime {
            height: 100,
            timestamp: 123,
        });

        cache
            .extend_summary_txs([(txid, summary)])
            .expect("extend should succeed");

        let merged = cache
            .get_tx(&txid)
            .expect("get should succeed")
            .expect("tx should exist");
        assert_eq!(merged.transaction, Some(raw_tx));
        assert_eq!(merged.received, 42);
        assert_eq!(merged.sent, 7);
        assert_eq!(merged.fee, Some(3));
        assert_eq!(
            merged.confirmation_time,
            Some(bdk::BlockTime {
                height: 100,
                timestamp: 123,
            })
        );
    }

    #[test]
    fn wallet_cache_extend_txs_clears_pending_lookup_entries() {
        let cache = WalletCache::new();
        let txid = Txid::all_zeros();

        cache
            .enqueue_pending_tx_lookup(txid)
            .expect("enqueue should succeed");
        let drained = cache
            .drain_pending_tx_lookups(10)
            .expect("drain should succeed");
        assert_eq!(drained, vec![txid]);

        cache
            .requeue_pending_tx_lookups(vec![txid])
            .expect("requeue should succeed");
        cache
            .extend_txs([(txid, tx_details(txid))])
            .expect("extend should succeed");

        let drained_after_insert = cache
            .drain_pending_tx_lookups(10)
            .expect("drain should succeed");
        assert!(drained_after_insert.is_empty());
    }

    #[test]
    fn wallet_cache_forced_lookup_drain_includes_requested_txid() {
        let cache = WalletCache::new();
        let required = Txid::all_zeros();
        let first = Txid::from_slice(&[1; 32]).expect("valid txid");
        let second = Txid::from_slice(&[2; 32]).expect("valid txid");

        cache
            .enqueue_pending_tx_lookup(first)
            .expect("enqueue should succeed");
        cache
            .enqueue_pending_tx_lookup(second)
            .expect("enqueue should succeed");

        let drained = cache
            .drain_pending_tx_lookups_including(required, 2)
            .expect("forced drain should succeed");
        assert_eq!(drained[0], required);
        assert_eq!(drained.len(), 2);
        assert!(drained.contains(&first) || drained.contains(&second));

        let remaining = cache
            .drain_pending_tx_lookups(10)
            .expect("drain should succeed");
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn wallet_cache_forced_tx_miss_drain_includes_requested_txid() {
        let cache = WalletCache::new();
        let required = Txid::all_zeros();
        let first = Txid::from_slice(&[1; 32]).expect("valid txid");
        let second = Txid::from_slice(&[2; 32]).expect("valid txid");

        cache
            .enqueue_pending_tx_miss(first)
            .expect("enqueue should succeed");
        cache
            .enqueue_pending_tx_miss(second)
            .expect("enqueue should succeed");

        let drained = cache
            .drain_pending_tx_misses_including(required, 2)
            .expect("forced drain should succeed");
        assert_eq!(drained[0], required);
        assert_eq!(drained.len(), 2);
        assert!(drained.contains(&first) || drained.contains(&second));

        let remaining = cache
            .drain_pending_tx_misses(10)
            .expect("drain should succeed");
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn cache_loaded_script_pubkeys_marks_mask_and_populates_paths() {
        let cache = WalletCache::new();
        let external_script = ScriptBuf::from(vec![0x51]);
        let internal_script = ScriptBuf::from(vec![0x52]);

        let loaded = SqlxWalletDb::cache_loaded_script_pubkeys(
            &cache,
            Some(KeychainKind::External),
            vec![(external_script.clone(), (KeychainKind::External, 7))],
        )
        .expect("cache should be populated");

        assert_eq!(loaded, vec![external_script.clone()]);
        assert!(cache.script_pubkeys_fully_loaded(Some(KeychainKind::External)));
        assert!(!cache.script_pubkeys_fully_loaded(Some(KeychainKind::Internal)));
        assert_eq!(
            cache
                .get_script_pubkey_path(external_script.as_script())
                .expect("path lookup should succeed"),
            Some((KeychainKind::External, 7))
        );

        SqlxWalletDb::cache_loaded_script_pubkeys(
            &cache,
            Some(KeychainKind::Internal),
            vec![(internal_script.clone(), (KeychainKind::Internal, 3))],
        )
        .expect("cache should be populated");

        assert!(cache.script_pubkeys_fully_loaded(None));
        let all = cache
            .all_script_pubkeys(None)
            .expect("all_script_pubkeys should succeed");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn wallet_cache_tracks_missing_txids_and_pending_drain() {
        let cache = WalletCache::new();
        let txid = Txid::all_zeros();
        let another = Txid::from_slice(&[1; 32]).expect("valid txid");

        cache
            .record_missing_txid(txid)
            .expect("mark should succeed");
        cache
            .enqueue_pending_tx_miss(txid)
            .expect("enqueue should succeed");
        cache
            .record_missing_txid(another)
            .expect("mark should succeed");
        cache
            .enqueue_pending_tx_miss(another)
            .expect("enqueue should succeed");
        assert!(cache
            .txid_marked_missing(&txid)
            .expect("lookup should succeed"));
        assert!(cache
            .should_batch_resolve_tx_misses(1)
            .expect("threshold check should succeed"));

        let drained = cache
            .drain_pending_tx_misses(1)
            .expect("drain should succeed");
        assert_eq!(drained.len(), 1);

        cache
            .mark_txid_not_missing(&txid)
            .expect("clear should succeed");
        assert!(!cache
            .txid_marked_missing(&txid)
            .expect("lookup should succeed"));
    }

    #[test]
    fn wallet_cache_tracks_missing_scripts_and_pending_drain() {
        let cache = WalletCache::new();
        let first = ScriptBuf::from(vec![0x51]);
        let second = ScriptBuf::from(vec![0x52]);

        cache
            .record_and_enqueue_missing_script(first.clone())
            .expect("mark should succeed");
        cache
            .record_and_enqueue_missing_script(second.clone())
            .expect("mark should succeed");
        assert!(cache
            .script_marked_missing(first.as_script())
            .expect("lookup should succeed"));
        assert!(cache
            .should_batch_resolve_script_misses(1)
            .expect("threshold check should succeed"));

        let drained = cache
            .drain_pending_script_misses(1)
            .expect("drain should succeed");
        assert_eq!(drained.len(), 1);

        cache
            .mark_script_not_missing(first.as_script())
            .expect("clear should succeed");
        assert!(!cache
            .script_marked_missing(first.as_script())
            .expect("lookup should succeed"));
    }

    #[test]
    fn wallet_cache_extend_summary_txs_clears_tx_miss_tracking() {
        let cache = WalletCache::new();
        let txid = Txid::all_zeros();

        cache
            .record_missing_txid(txid)
            .expect("mark should succeed");
        cache
            .enqueue_pending_tx_miss(txid)
            .expect("enqueue should succeed");
        assert!(cache
            .txid_marked_missing(&txid)
            .expect("lookup should succeed"));

        cache
            .extend_summary_txs([(txid, tx_details(txid))])
            .expect("extend should succeed");

        assert!(!cache
            .txid_marked_missing(&txid)
            .expect("lookup should succeed"));
        let drained = cache
            .drain_pending_tx_misses(1)
            .expect("drain should succeed");
        assert!(drained.is_empty());
    }
}
