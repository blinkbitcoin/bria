use bdk::{
    bitcoin::{Script, ScriptBuf, Txid},
    KeychainKind, TransactionDetails,
};
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use super::{ScriptPubkeyCache, SqlxWalletDb, TransactionCache};

#[derive(Clone)]
pub(super) struct WalletCache {
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
}

impl WalletCache {
    fn clear_even_if_poisoned<T>(mutex: &Mutex<T>)
    where
        T: Default,
    {
        let mut guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
        *guard = T::default();
        mutex.clear_poison();
    }

    pub(super) fn new() -> Self {
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
        }
    }

    fn lock_with_error<'a, T>(
        &self,
        mutex: &'a Mutex<T>,
        context: &'static str,
    ) -> Result<MutexGuard<'a, T>, bdk::Error> {
        mutex
            .lock()
            .map_err(|_| bdk::Error::Generic(format!("{context} lock poisoned")))
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
        self.lock_with_error(&self.script_pubkeys, "script pubkeys cache")
    }

    fn lock_transactions(&self) -> Result<MutexGuard<'_, TransactionCache>, bdk::Error> {
        self.lock_with_error(&self.transactions, "transactions cache")
    }

    fn lock_missing_script_pubkeys(
        &self,
    ) -> Result<MutexGuard<'_, HashSet<ScriptBuf>>, bdk::Error> {
        self.lock_with_error(&self.missing_script_pubkeys, "missing script pubkeys cache")
    }

    fn lock_missing_txids(&self) -> Result<MutexGuard<'_, HashSet<Txid>>, bdk::Error> {
        self.lock_with_error(&self.missing_txids, "missing txids cache")
    }

    fn lock_pending_script_misses(&self) -> Result<MutexGuard<'_, HashSet<ScriptBuf>>, bdk::Error> {
        self.lock_with_error(&self.pending_script_misses, "pending script misses cache")
    }

    fn lock_pending_tx_misses(&self) -> Result<MutexGuard<'_, HashSet<Txid>>, bdk::Error> {
        self.lock_with_error(&self.pending_tx_misses, "pending tx misses cache")
    }

    pub(super) fn get_script_pubkey_path(
        &self,
        script: &Script,
    ) -> Result<Option<(KeychainKind, u32)>, bdk::Error> {
        let cache = self.lock_script_pubkeys()?;
        Ok(cache.get(script).copied())
    }

    pub(super) fn insert_script_pubkey(
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

    pub(super) fn extend_script_pubkeys<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (ScriptBuf, (KeychainKind, u32))>,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        self.clear_script_miss_tracking(entries.iter().map(|(script, _)| script))?;
        let mut cache = self.lock_script_pubkeys()?;
        cache.extend(entries);
        Ok(())
    }

    pub(super) fn all_script_pubkeys(
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

    pub(super) fn script_pubkeys_fully_loaded(&self, keychain: Option<KeychainKind>) -> bool {
        let required_mask = Self::script_pubkey_mask_for(keychain);
        let loaded_mask = self.script_pubkeys_loaded_mask.load(Ordering::Acquire);
        loaded_mask & required_mask == required_mask
    }

    pub(super) fn mark_script_pubkeys_loaded(&self, keychain: Option<KeychainKind>) {
        let mask = Self::script_pubkey_mask_for(keychain);
        self.script_pubkeys_loaded_mask
            .fetch_or(mask, Ordering::Release);
    }

    pub(super) fn get_tx(&self, txid: &Txid) -> Result<Option<TransactionDetails>, bdk::Error> {
        let cache = self.lock_transactions()?;
        Ok(cache.get(txid).cloned())
    }

    pub(super) fn insert_tx(&self, txid: Txid, tx: TransactionDetails) -> Result<(), bdk::Error> {
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

    pub(super) fn extend_txs<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (Txid, TransactionDetails)>,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        self.clear_tx_miss_tracking(entries.iter().map(|(txid, _)| txid))?;
        let mut cache = self.lock_transactions()?;
        cache.extend(entries);
        Ok(())
    }

    pub(super) fn extend_summary_txs<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (Txid, TransactionDetails)>,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        self.clear_tx_miss_tracking(entries.iter().map(|(txid, _)| txid))?;
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

    pub(super) fn all_txs(&self) -> Result<Vec<TransactionDetails>, bdk::Error> {
        let cache = self.lock_transactions()?;
        Ok(cache.values().cloned().collect())
    }

    pub(super) fn all_summary_txs(&self) -> Result<HashMap<Txid, TransactionDetails>, bdk::Error> {
        let cache = self.lock_transactions()?;
        Ok(cache
            .values()
            .map(|tx| (tx.txid, SqlxWalletDb::summary_tx_from_ref(tx)))
            .collect())
    }

    pub(super) fn raw_txs_fully_loaded(&self) -> bool {
        self.raw_txs_fully_loaded.load(Ordering::Acquire)
    }

    pub(super) fn set_raw_txs_fully_loaded(&self) {
        self.raw_txs_fully_loaded.store(true, Ordering::Release);
        self.summary_txs_fully_loaded.store(true, Ordering::Release);
    }

    pub(super) fn summary_txs_fully_loaded(&self) -> bool {
        self.summary_txs_fully_loaded.load(Ordering::Acquire)
    }

    pub(super) fn set_summary_txs_fully_loaded(&self) {
        self.summary_txs_fully_loaded.store(true, Ordering::Release);
    }

    pub(super) fn remove_tx(&self, txid: &Txid) -> Result<(), bdk::Error> {
        {
            let mut cache = self.lock_transactions()?;
            cache.remove(txid);
        }
        self.record_missing_txid(*txid)?;
        Ok(())
    }

    pub(super) fn script_marked_missing(&self, script: &Script) -> Result<bool, bdk::Error> {
        let missing = self.lock_missing_script_pubkeys()?;
        Ok(missing.contains(script))
    }

    pub(super) fn record_missing_script(&self, script: ScriptBuf) -> Result<(), bdk::Error> {
        self.lock_missing_script_pubkeys()?.insert(script);
        Ok(())
    }

    pub(super) fn record_and_enqueue_missing_script(
        &self,
        script: ScriptBuf,
    ) -> Result<(), bdk::Error> {
        self.lock_missing_script_pubkeys()?.insert(script.clone());
        self.lock_pending_script_misses()?.insert(script);
        Ok(())
    }

    pub(super) fn mark_script_not_missing(&self, script: &Script) -> Result<(), bdk::Error> {
        self.lock_missing_script_pubkeys()?.remove(script);
        self.lock_pending_script_misses()?.remove(script);
        Ok(())
    }

    pub(super) fn drain_pending_script_misses(
        &self,
        max: usize,
    ) -> Result<Vec<ScriptBuf>, bdk::Error> {
        let mut pending = self.lock_pending_script_misses()?;
        let drained: Vec<_> = pending.iter().take(max).cloned().collect();
        for script in &drained {
            pending.remove(script);
        }
        Ok(drained)
    }

    pub(super) fn txid_marked_missing(&self, txid: &Txid) -> Result<bool, bdk::Error> {
        let missing = self.lock_missing_txids()?;
        Ok(missing.contains(txid))
    }

    pub(super) fn record_missing_txid(&self, txid: Txid) -> Result<(), bdk::Error> {
        self.lock_missing_txids()?.insert(txid);
        Ok(())
    }

    pub(super) fn record_and_enqueue_missing_txid(&self, txid: Txid) -> Result<(), bdk::Error> {
        self.lock_missing_txids()?.insert(txid);
        self.lock_pending_tx_misses()?.insert(txid);
        Ok(())
    }

    pub(super) fn mark_txid_not_missing(&self, txid: &Txid) -> Result<(), bdk::Error> {
        self.lock_missing_txids()?.remove(txid);
        self.lock_pending_tx_misses()?.remove(txid);
        Ok(())
    }

    pub(super) fn drain_pending_tx_misses(&self, max: usize) -> Result<Vec<Txid>, bdk::Error> {
        let mut pending = self.lock_pending_tx_misses()?;
        let drained: Vec<_> = pending.iter().take(max).copied().collect();
        for txid in &drained {
            pending.remove(txid);
        }
        Ok(drained)
    }

    pub(super) fn should_batch_resolve_script_misses(
        &self,
        threshold: usize,
    ) -> Result<bool, bdk::Error> {
        let pending = self.lock_pending_script_misses()?;
        Ok(pending.len() >= threshold)
    }

    pub(super) fn should_batch_resolve_tx_misses(
        &self,
        threshold: usize,
    ) -> Result<bool, bdk::Error> {
        let pending = self.lock_pending_tx_misses()?;
        Ok(pending.len() >= threshold)
    }

    pub(super) fn invalidate(&self) {
        Self::clear_even_if_poisoned(&self.script_pubkeys);
        Self::clear_even_if_poisoned(&self.transactions);
        Self::clear_even_if_poisoned(&self.missing_script_pubkeys);
        Self::clear_even_if_poisoned(&self.missing_txids);
        Self::clear_even_if_poisoned(&self.pending_script_misses);
        Self::clear_even_if_poisoned(&self.pending_tx_misses);

        self.script_pubkeys_loaded_mask.store(0, Ordering::Release);
        self.raw_txs_fully_loaded.store(false, Ordering::Release);
        self.summary_txs_fully_loaded
            .store(false, Ordering::Release);
    }
}
