mod convert;
mod db_traits;
mod descriptor_checksum;
mod index;
mod script_pubkeys;
mod sync_times;
mod transactions;
mod utxos;

use bdk::{
    bitcoin::{Script, ScriptBuf, Txid},
    KeychainKind, LocalUtxo, TransactionDetails,
};
use sqlx::PgPool;
use tokio::runtime::Handle;

use crate::primitives::*;
use descriptor_checksum::DescriptorChecksums;
use index::Indexes;
use script_pubkeys::ScriptPubkeys;
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    sync::{Arc, Mutex, MutexGuard},
};
pub(super) use sync_times::SyncTimes;
pub use transactions::*;
pub use utxos::*;

type ScriptPubkeyCache = HashMap<ScriptBuf, (KeychainKind, u32)>;
type TransactionCache = HashMap<Txid, TransactionDetails>;

#[derive(Copy, Clone, Eq, PartialEq)]
enum TxLookupMode {
    Any,
    RequireRaw,
}

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

#[derive(Clone)]
struct WalletCache {
    script_pubkeys: Arc<Mutex<ScriptPubkeyCache>>,
    transactions: Arc<Mutex<TransactionCache>>,
    missing_script_pubkeys: Arc<Mutex<HashSet<ScriptBuf>>>,
    missing_txids: Arc<Mutex<HashSet<Txid>>>,
    pending_script_misses: Arc<Mutex<HashSet<ScriptBuf>>>,
    pending_tx_misses: Arc<Mutex<HashSet<Txid>>>,
    // Process-local hint for which keychain script path sets are fully hydrated.
    // Bit 0: external, bit 1: internal.
    // This is intentionally not synchronized across processes.
    script_pubkeys_loaded_mask: Arc<AtomicU8>,
    // Process-local hint: true means this instance has already hydrated raw tx details
    // from the DB at least once. It is intentionally not synchronized across processes.
    raw_txs_fully_loaded: Arc<AtomicBool>,
    // Process-local hint: true means summary tx details were fully hydrated once.
    summary_txs_fully_loaded: Arc<AtomicBool>,
    raw_tx_lookup_miss_count: Arc<AtomicUsize>,
    script_lookup_miss_count: Arc<AtomicUsize>,
}

impl WalletCache {
    fn new() -> Self {
        Self {
            script_pubkeys: Arc::new(Mutex::new(HashMap::new())),
            transactions: Arc::new(Mutex::new(HashMap::new())),
            missing_script_pubkeys: Arc::new(Mutex::new(HashSet::new())),
            missing_txids: Arc::new(Mutex::new(HashSet::new())),
            pending_script_misses: Arc::new(Mutex::new(HashSet::new())),
            pending_tx_misses: Arc::new(Mutex::new(HashSet::new())),
            script_pubkeys_loaded_mask: Arc::new(AtomicU8::new(0)),
            raw_txs_fully_loaded: Arc::new(AtomicBool::new(false)),
            summary_txs_fully_loaded: Arc::new(AtomicBool::new(false)),
            raw_tx_lookup_miss_count: Arc::new(AtomicUsize::new(0)),
            script_lookup_miss_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn script_pubkey_mask_for(keychain: Option<KeychainKind>) -> u8 {
        const EXTERNAL: u8 = 1;
        const INTERNAL: u8 = 2;
        match keychain {
            Some(KeychainKind::External) => EXTERNAL,
            Some(KeychainKind::Internal) => INTERNAL,
            None => EXTERNAL | INTERNAL,
        }
    }

    fn lock_script_pubkeys(&self) -> Result<MutexGuard<'_, ScriptPubkeyCache>, bdk::Error> {
        self.script_pubkeys
            .lock()
            .map_err(|_| bdk::Error::Generic("script pubkeys cache lock poisoned".to_string()))
    }

    fn lock_transactions(&self) -> Result<MutexGuard<'_, TransactionCache>, bdk::Error> {
        self.transactions
            .lock()
            .map_err(|_| bdk::Error::Generic("transactions cache lock poisoned".to_string()))
    }

    fn lock_missing_script_pubkeys(
        &self,
    ) -> Result<MutexGuard<'_, HashSet<ScriptBuf>>, bdk::Error> {
        self.missing_script_pubkeys.lock().map_err(|_| {
            bdk::Error::Generic("missing script pubkeys cache lock poisoned".to_string())
        })
    }

    fn lock_missing_txids(&self) -> Result<MutexGuard<'_, HashSet<Txid>>, bdk::Error> {
        self.missing_txids
            .lock()
            .map_err(|_| bdk::Error::Generic("missing txids cache lock poisoned".to_string()))
    }

    fn lock_pending_script_misses(&self) -> Result<MutexGuard<'_, HashSet<ScriptBuf>>, bdk::Error> {
        self.pending_script_misses.lock().map_err(|_| {
            bdk::Error::Generic("pending script misses cache lock poisoned".to_string())
        })
    }

    fn lock_pending_tx_misses(&self) -> Result<MutexGuard<'_, HashSet<Txid>>, bdk::Error> {
        self.pending_tx_misses
            .lock()
            .map_err(|_| bdk::Error::Generic("pending tx misses cache lock poisoned".to_string()))
    }

    fn get_script_pubkey_path(
        &self,
        script: &Script,
    ) -> Result<Option<(KeychainKind, u32)>, bdk::Error> {
        let cache = self.lock_script_pubkeys()?;
        Ok(cache.get(script).copied())
    }

    fn insert_script_pubkey(
        &self,
        script: ScriptBuf,
        path: (KeychainKind, u32),
    ) -> Result<(), bdk::Error> {
        {
            let mut cache = self.lock_script_pubkeys()?;
            cache.insert(script.clone(), path);
        }
        self.mark_script_not_missing(&script)?;
        Ok(())
    }

    fn clear_script_miss_tracking<'a, I>(&self, scripts: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = &'a ScriptBuf>,
    {
        let mut missing = self.lock_missing_script_pubkeys()?;
        let mut pending = self.lock_pending_script_misses()?;
        for script in scripts {
            missing.remove(script.as_script());
            pending.remove(script);
        }
        Ok(())
    }

    fn extend_script_pubkeys<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (ScriptBuf, (KeychainKind, u32))>,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        self.clear_script_miss_tracking(entries.iter().map(|(script, _)| script))?;
        let mut cache = self.lock_script_pubkeys()?;
        cache.extend(entries);
        Ok(())
    }

    fn all_script_pubkeys(
        &self,
        keychain: Option<KeychainKind>,
    ) -> Result<Vec<ScriptBuf>, bdk::Error> {
        let cache = self.lock_script_pubkeys()?;
        Ok(cache
            .iter()
            .filter(|(_, (kind, _))| keychain.is_none_or(|k| *kind == k))
            .map(|(script, _)| script.clone())
            .collect())
    }

    fn script_pubkeys_fully_loaded(&self, keychain: Option<KeychainKind>) -> bool {
        let required_mask = Self::script_pubkey_mask_for(keychain);
        let loaded_mask = self.script_pubkeys_loaded_mask.load(Ordering::Acquire);
        loaded_mask & required_mask == required_mask
    }

    fn mark_script_pubkeys_loaded(&self, keychain: Option<KeychainKind>) {
        let mask = Self::script_pubkey_mask_for(keychain);
        self.script_pubkeys_loaded_mask
            .fetch_or(mask, Ordering::Release);
    }

    fn get_tx(&self, txid: &Txid) -> Result<Option<TransactionDetails>, bdk::Error> {
        let cache = self.lock_transactions()?;
        Ok(cache.get(txid).cloned())
    }

    fn insert_tx(&self, txid: Txid, tx: TransactionDetails) -> Result<(), bdk::Error> {
        {
            let mut cache = self.lock_transactions()?;
            cache.insert(txid, tx);
        }
        self.mark_txid_not_missing(&txid)?;
        Ok(())
    }

    fn clear_tx_miss_tracking<'a, I>(&self, txids: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = &'a Txid>,
    {
        let mut missing = self.lock_missing_txids()?;
        let mut pending = self.lock_pending_tx_misses()?;
        for txid in txids {
            missing.remove(txid);
            pending.remove(txid);
        }
        Ok(())
    }

    fn extend_txs<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (Txid, TransactionDetails)>,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        self.clear_tx_miss_tracking(entries.iter().map(|(txid, _)| txid))?;
        let mut cache = self.lock_transactions()?;
        cache.extend(entries);
        Ok(())
    }

    fn extend_summary_txs<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (Txid, TransactionDetails)>,
    {
        let mut cache = self.lock_transactions()?;
        for (txid, mut summary) in entries {
            // Summary refreshes may run after raw tx bytes were already hydrated. Preserve any
            // cached raw transaction payload while applying fresh DB metadata fields.
            // `iter_txs` overlays in-memory batch writes afterwards, so uncommitted updates still
            // take precedence in returned views.
            if let Some(raw_tx) = cache
                .get(&txid)
                .and_then(|existing| existing.transaction.clone())
            {
                summary.transaction = Some(raw_tx);
            }
            cache.insert(txid, summary);
        }
        Ok(())
    }

    fn all_txs(&self) -> Result<Vec<TransactionDetails>, bdk::Error> {
        let cache = self.lock_transactions()?;
        Ok(cache.values().cloned().collect())
    }

    fn all_summary_txs(&self) -> Result<HashMap<Txid, TransactionDetails>, bdk::Error> {
        let cache = self.lock_transactions()?;
        Ok(cache
            .values()
            .map(|tx| (tx.txid, SqlxWalletDb::summary_tx_from_ref(tx)))
            .collect())
    }

    fn raw_txs_fully_loaded(&self) -> bool {
        self.raw_txs_fully_loaded.load(Ordering::Acquire)
    }

    fn set_raw_txs_fully_loaded(&self) {
        self.raw_txs_fully_loaded.store(true, Ordering::Release);
        self.summary_txs_fully_loaded.store(true, Ordering::Release);
    }

    fn summary_txs_fully_loaded(&self) -> bool {
        self.summary_txs_fully_loaded.load(Ordering::Acquire)
    }

    fn set_summary_txs_fully_loaded(&self) {
        self.summary_txs_fully_loaded.store(true, Ordering::Release);
    }

    fn remove_tx(&self, txid: &Txid) -> Result<(), bdk::Error> {
        {
            let mut cache = self.lock_transactions()?;
            cache.remove(txid);
        }
        self.mark_txid_missing(*txid)?;
        Ok(())
    }

    fn script_marked_missing(&self, script: &Script) -> Result<bool, bdk::Error> {
        let missing = self.lock_missing_script_pubkeys()?;
        Ok(missing.contains(script))
    }

    fn mark_script_missing(&self, script: ScriptBuf) -> Result<(), bdk::Error> {
        self.lock_missing_script_pubkeys()?.insert(script.clone());
        self.lock_pending_script_misses()?.insert(script);
        self.script_lookup_miss_count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn mark_script_not_missing(&self, script: &Script) -> Result<(), bdk::Error> {
        self.lock_missing_script_pubkeys()?.remove(script);
        self.lock_pending_script_misses()?.remove(script);
        Ok(())
    }

    fn drain_pending_script_misses(&self, max: usize) -> Result<Vec<ScriptBuf>, bdk::Error> {
        let mut pending = self.lock_pending_script_misses()?;
        let drained: Vec<_> = pending.iter().take(max).cloned().collect();
        for script in &drained {
            pending.remove(script);
        }
        Ok(drained)
    }

    fn txid_marked_missing(&self, txid: &Txid) -> Result<bool, bdk::Error> {
        let missing = self.lock_missing_txids()?;
        Ok(missing.contains(txid))
    }

    fn mark_txid_missing(&self, txid: Txid) -> Result<(), bdk::Error> {
        self.lock_missing_txids()?.insert(txid);
        self.lock_pending_tx_misses()?.insert(txid);
        self.raw_tx_lookup_miss_count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn mark_txid_not_missing(&self, txid: &Txid) -> Result<(), bdk::Error> {
        self.lock_missing_txids()?.remove(txid);
        self.lock_pending_tx_misses()?.remove(txid);
        Ok(())
    }

    fn drain_pending_tx_misses(&self, max: usize) -> Result<Vec<Txid>, bdk::Error> {
        let mut pending = self.lock_pending_tx_misses()?;
        let drained: Vec<_> = pending.iter().take(max).copied().collect();
        for txid in &drained {
            pending.remove(txid);
        }
        Ok(drained)
    }

    fn should_batch_resolve_script_misses(&self, threshold: usize) -> bool {
        self.script_lookup_miss_count.load(Ordering::Acquire) >= threshold
    }

    fn should_batch_resolve_tx_misses(&self, threshold: usize) -> bool {
        self.raw_tx_lookup_miss_count.load(Ordering::Acquire) >= threshold
    }

    fn reset_script_miss_counter(&self) {
        self.script_lookup_miss_count.store(0, Ordering::Release);
    }

    fn reset_tx_miss_counter(&self) {
        self.raw_tx_lookup_miss_count.store(0, Ordering::Release);
    }
}

pub struct SqlxWalletDb {
    ctx: WalletDbContext,
    cache: WalletCache,
    batch: WalletBatchState,
}

impl SqlxWalletDb {
    const MISS_BATCH_THRESHOLD: usize = 64;
    const MISS_BATCH_SIZE: usize = 512;

    fn unsupported_operation(operation: &str) -> bdk::Error {
        bdk::Error::Generic(format!("{operation} is not supported by SqlxWalletDb"))
    }

    pub fn new(pool: PgPool, keychain_id: KeychainId) -> Self {
        Self {
            ctx: WalletDbContext::new(pool, keychain_id),
            cache: WalletCache::new(),
            batch: WalletBatchState::default(),
        }
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

    fn resolve_pending_script_misses(&self) -> Result<(), bdk::Error> {
        if !self
            .cache
            .should_batch_resolve_script_misses(Self::MISS_BATCH_THRESHOLD)
        {
            return Ok(());
        }

        let pending = self
            .cache
            .drain_pending_script_misses(Self::MISS_BATCH_SIZE)?;
        if pending.is_empty() {
            return Ok(());
        }

        let found = self.ctx.rt.block_on(async {
            self.script_pubkeys_repo()
                .find_paths_for_scripts(&pending)
                .await
        })?;

        if !found.is_empty() {
            self.cache.extend_script_pubkeys(
                found
                    .into_iter()
                    .map(|(script, (kind, path))| (script, (KeychainKind::from(kind), path))),
            )?;
        }

        self.cache.reset_script_miss_counter();
        Ok(())
    }

    fn resolve_pending_tx_misses(&self) -> Result<(), bdk::Error> {
        if !self
            .cache
            .should_batch_resolve_tx_misses(Self::MISS_BATCH_THRESHOLD)
        {
            return Ok(());
        }

        let pending = self.cache.drain_pending_tx_misses(Self::MISS_BATCH_SIZE)?;
        if pending.is_empty() {
            return Ok(());
        }

        let found = self
            .ctx
            .rt
            .block_on(async { self.transactions_repo().find_by_ids(&pending).await })?;

        if !found.is_empty() {
            self.cache.extend_txs(found)?;
        }

        self.cache.reset_tx_miss_counter();
        Ok(())
    }

    fn lookup_script_pubkey_path(
        &self,
        script: &Script,
    ) -> Result<(Option<(KeychainKind, u32)>, &'static str), bdk::Error> {
        if let Some(path) = self.batch.addresses.get(script) {
            return Ok((Some(*path), "batch"));
        }

        if let Some(path) = self.cache.get_script_pubkey_path(script)? {
            return Ok((Some(path), "cache"));
        }

        if self.cache.script_marked_missing(script)? {
            tracing::trace!("script path miss cache hit");
            return Ok((None, "miss_cache"));
        }

        // Once both keychains are fully hydrated in this process, a cache miss is definitive.
        if self.cache.script_pubkeys_fully_loaded(None) {
            self.cache.mark_script_missing(script.to_owned())?;
            return Ok((None, "fully_loaded_miss"));
        }

        self.resolve_pending_script_misses()?;
        if let Some(path) = self.cache.get_script_pubkey_path(script)? {
            return Ok((Some(path), "batch_resolve"));
        }

        let script_pubkey = script.to_owned();
        let found = self
            .ctx
            .rt
            .block_on(async { self.script_pubkeys_repo().find_path(&script_pubkey).await })?;

        if let Some((kind, path)) = found {
            let value = (KeychainKind::from(kind), path);
            self.cache.insert_script_pubkey(script_pubkey, value)?;
            return Ok((Some(value), "db_hit"));
        }

        self.cache.mark_script_missing(script_pubkey)?;

        Ok((None, "db_miss"))
    }

    fn lookup_tx_with_mode(
        &self,
        txid: &Txid,
        mode: TxLookupMode,
    ) -> Result<(Option<TransactionDetails>, &'static str), bdk::Error> {
        if let Some(tx) = self.batch.txs.get(txid) {
            if Self::tx_matches_lookup_mode(tx, mode) {
                return Ok((Some(tx.clone()), "batch"));
            }

            return Ok((None, "batch_mode_miss"));
        }

        if let Some(tx) = self.cache.get_tx(txid)? {
            if Self::tx_matches_lookup_mode(&tx, mode) {
                return Ok((Some(tx), "cache"));
            }

            if self.cache.raw_txs_fully_loaded() {
                return Ok((None, "cache_mode_miss"));
            }
        }

        if self.cache.txid_marked_missing(txid)? {
            tracing::trace!("tx miss cache hit");
            return Ok((None, "miss_cache"));
        }

        // Once raw txs are fully loaded in this process, a miss is definitive.
        if self.cache.raw_txs_fully_loaded() {
            self.cache.mark_txid_missing(*txid)?;
            return Ok((None, "fully_loaded_miss"));
        }

        self.resolve_pending_tx_misses()?;
        if let Some(tx) = self.cache.get_tx(txid)? {
            if Self::tx_matches_lookup_mode(&tx, mode) {
                return Ok((Some(tx), "batch_resolve"));
            }
            if self.cache.raw_txs_fully_loaded() {
                self.cache.mark_txid_missing(*txid)?;
                return Ok((None, "batch_resolve_mode_miss"));
            }
        }

        let found = self
            .ctx
            .rt
            .block_on(async { self.transactions_repo().find_by_id(txid).await })?;

        // DB rows represent persisted TransactionDetails; this store does not persist a
        // "summary-only" transaction format. A DB hit is therefore valid for both lookup
        // modes (`Any` and `RequireRaw`).

        if let Some(tx) = &found {
            self.cache.insert_tx(tx.txid, tx.clone())?;
            return Ok((found, "db_hit"));
        } else {
            self.cache.mark_txid_missing(*txid)?;
            return Ok((None, "db_miss"));
        }
    }

    fn lookup_tx(
        &self,
        txid: &Txid,
    ) -> Result<(Option<TransactionDetails>, &'static str), bdk::Error> {
        self.lookup_tx_with_mode(txid, TxLookupMode::Any)
    }

    fn tx_matches_lookup_mode(tx: &TransactionDetails, mode: TxLookupMode) -> bool {
        mode == TxLookupMode::Any || tx.transaction.is_some()
    }

    fn summary_tx_from_ref(tx: &TransactionDetails) -> TransactionDetails {
        TransactionDetails {
            transaction: None,
            txid: tx.txid,
            received: tx.received,
            sent: tx.sent,
            fee: tx.fee,
            confirmation_time: tx.confirmation_time.clone(),
        }
    }

    fn summary_tx_from_owned(tx: TransactionDetails) -> TransactionDetails {
        let TransactionDetails {
            txid,
            received,
            sent,
            fee,
            confirmation_time,
            ..
        } = tx;

        TransactionDetails {
            transaction: None,
            txid,
            received,
            sent,
            fee,
            confirmation_time,
        }
    }

    fn overlay_batch_txs(
        mut txs: HashMap<Txid, TransactionDetails>,
        batch_txs: &HashMap<Txid, TransactionDetails>,
        include_raw: bool,
    ) -> HashMap<Txid, TransactionDetails> {
        if include_raw {
            txs.extend(batch_txs.iter().map(|(id, tx)| (*id, tx.clone())));
        } else {
            txs.extend(
                batch_txs
                    .iter()
                    .map(|(id, tx)| (*id, Self::summary_tx_from_ref(tx))),
            );
        }

        txs
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
            .insert_tx(txid, details.clone())
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
            .insert_tx(txid, tx_details(txid))
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
            .insert_tx(txid, existing)
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

        cache.mark_txid_missing(txid).expect("mark should succeed");
        cache
            .mark_txid_missing(another)
            .expect("mark should succeed");
        assert!(cache
            .txid_marked_missing(&txid)
            .expect("lookup should succeed"));
        assert!(cache.should_batch_resolve_tx_misses(1));

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
            .mark_script_missing(first.clone())
            .expect("mark should succeed");
        cache
            .mark_script_missing(second.clone())
            .expect("mark should succeed");
        assert!(cache
            .script_marked_missing(first.as_script())
            .expect("lookup should succeed"));
        assert!(cache.should_batch_resolve_script_misses(1));

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
}
