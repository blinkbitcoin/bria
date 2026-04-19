use bdk::{
    bitcoin::{Script, ScriptBuf, Txid},
    KeychainKind, TransactionDetails,
};
use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use super::{ScriptPubkeyCache, SqlxWalletDb, TransactionCache};

struct Tracked<T>(Mutex<T>);

impl<T> Tracked<T> {
    fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }

    fn lock(&self, context: &'static str) -> Result<MutexGuard<'_, T>, bdk::Error> {
        self.0
            .lock()
            .map_err(|_| bdk::Error::Generic(format!("{context} lock poisoned")))
    }
}

impl<T> Tracked<T>
where
    T: Default,
{
    fn clear_even_if_poisoned(&self) {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *guard = T::default();
        self.0.clear_poison();
    }
}

struct PendingSet<T> {
    inner: Tracked<HashSet<T>>,
    context: &'static str,
}

impl<T> PendingSet<T>
where
    T: Eq + Hash,
{
    fn new(context: &'static str) -> Self {
        Self {
            inner: Tracked::new(HashSet::new()),
            context,
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashSet<T>>, bdk::Error> {
        self.inner.lock(self.context)
    }

    fn insert(&self, value: T) -> Result<(), bdk::Error> {
        self.inner.lock(self.context)?.insert(value);
        Ok(())
    }

    fn contains<Q>(&self, value: &Q) -> Result<bool, bdk::Error>
    where
        T: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        Ok(self.inner.lock(self.context)?.contains(value))
    }

    fn drain(&self, max: usize) -> Result<Vec<T>, bdk::Error>
    where
        T: Clone,
    {
        let mut pending = self.inner.lock(self.context)?;
        let drained: Vec<_> = pending.iter().take(max).cloned().collect();
        for value in &drained {
            pending.remove(value);
        }
        Ok(drained)
    }

    fn requeue<I>(&self, values: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = T>,
    {
        self.inner.lock(self.context)?.extend(values);
        Ok(())
    }

    fn should_batch(&self, threshold: usize) -> Result<bool, bdk::Error> {
        Ok(self.inner.lock(self.context)?.len() >= threshold)
    }

    fn clear_even_if_poisoned(&self) {
        self.inner.clear_even_if_poisoned();
    }
}

struct MissTracker<T> {
    confirmed: Tracked<HashSet<T>>,
    confirmed_context: &'static str,
    pending: PendingSet<T>,
}

impl<T> MissTracker<T>
where
    T: Eq + Hash,
{
    fn new(confirmed_context: &'static str, pending_context: &'static str) -> Self {
        Self {
            confirmed: Tracked::new(HashSet::new()),
            confirmed_context,
            pending: PendingSet::new(pending_context),
        }
    }

    fn is_missing<Q>(&self, value: &Q) -> Result<bool, bdk::Error>
    where
        T: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        Ok(self.confirmed.lock(self.confirmed_context)?.contains(value))
    }

    fn record(&self, value: T) -> Result<(), bdk::Error> {
        self.confirmed.lock(self.confirmed_context)?.insert(value);
        Ok(())
    }

    fn enqueue_pending(&self, value: T) -> Result<(), bdk::Error> {
        self.pending.insert(value)
    }

    fn pending_contains<Q>(&self, value: &Q) -> Result<bool, bdk::Error>
    where
        T: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.pending.contains(value)
    }

    fn record_and_enqueue(&self, value: T) -> Result<(), bdk::Error>
    where
        T: Clone,
    {
        let (mut confirmed, mut pending) = self.lock_both()?;
        confirmed.insert(value.clone());
        pending.insert(value);
        Ok(())
    }

    fn clear<Q>(&self, value: &Q) -> Result<(), bdk::Error>
    where
        T: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let (mut confirmed, mut pending) = self.lock_both()?;
        confirmed.remove(value);
        pending.remove(value);
        Ok(())
    }

    fn clear_many<'a, I>(&self, values: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = &'a T>,
        T: 'a,
    {
        let (mut confirmed, mut pending) = self.lock_both()?;
        for value in values {
            confirmed.remove(value);
            pending.remove(value);
        }
        Ok(())
    }

    // LOCK ORDER INVARIANT: always acquire confirmed before pending.
    // Use this method whenever both guards are needed simultaneously.
    fn lock_both(
        &self,
    ) -> Result<(MutexGuard<'_, HashSet<T>>, MutexGuard<'_, HashSet<T>>), bdk::Error> {
        let confirmed = self.confirmed.lock(self.confirmed_context)?;
        let pending = self.pending.lock()?;
        Ok((confirmed, pending))
    }

    fn lock_pending(&self) -> Result<MutexGuard<'_, HashSet<T>>, bdk::Error> {
        self.pending.lock()
    }

    fn drain_pending(&self, max: usize) -> Result<Vec<T>, bdk::Error>
    where
        T: Clone,
    {
        self.pending.drain(max)
    }

    fn requeue_pending<I>(&self, values: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = T>,
    {
        self.pending.requeue(values)
    }

    fn should_batch_pending(&self, threshold: usize) -> Result<bool, bdk::Error> {
        self.pending.should_batch(threshold)
    }

    fn clear_even_if_poisoned(&self) {
        self.confirmed.clear_even_if_poisoned();
        self.pending.clear_even_if_poisoned();
    }
}

#[derive(Clone)]
pub(super) struct WalletCache {
    script_pubkeys: Arc<Tracked<ScriptPubkeyCache>>,
    transactions: Arc<Tracked<TransactionCache>>,
    script_misses: Arc<MissTracker<ScriptBuf>>,
    tx_misses: Arc<MissTracker<Txid>>,
    // Txids not yet seen in the in-process cache and not yet known-missing. These are batched
    // before we fall back to recording a miss.
    pending_tx_lookups: Arc<Tracked<HashSet<Txid>>>,
    // Tracks txids that already consumed their one targeted miss-cache retry in this
    // SqlxWalletDb lifetime, while still allowing later threshold-driven batch retries.
    forced_tx_miss_retries: Arc<Tracked<HashSet<Txid>>>,
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
    pub(super) fn new() -> Self {
        Self {
            script_pubkeys: Arc::new(Tracked::new(HashMap::new())),
            transactions: Arc::new(Tracked::new(HashMap::new())),
            script_misses: Arc::new(MissTracker::new(
                "missing script pubkeys cache",
                "pending script misses cache",
            )),
            tx_misses: Arc::new(MissTracker::new(
                "missing txids cache",
                "pending tx misses cache",
            )),
            pending_tx_lookups: Arc::new(Tracked::new(HashSet::new())),
            forced_tx_miss_retries: Arc::new(Tracked::new(HashSet::new())),
            script_pubkeys_loaded_mask: Arc::new(AtomicU8::new(0)),
            raw_txs_fully_loaded: Arc::new(AtomicBool::new(false)),
            summary_txs_fully_loaded: Arc::new(AtomicBool::new(false)),
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
        self.script_pubkeys.lock("script pubkeys cache")
    }

    fn lock_transactions(&self) -> Result<MutexGuard<'_, TransactionCache>, bdk::Error> {
        self.transactions.lock("transactions cache")
    }

    fn lock_pending_tx_misses(&self) -> Result<MutexGuard<'_, HashSet<Txid>>, bdk::Error> {
        self.tx_misses.lock_pending()
    }

    fn lock_pending_tx_lookups(&self) -> Result<MutexGuard<'_, HashSet<Txid>>, bdk::Error> {
        self.pending_tx_lookups.lock("pending tx lookups cache")
    }

    fn lock_forced_tx_miss_retries(&self) -> Result<MutexGuard<'_, HashSet<Txid>>, bdk::Error> {
        self.forced_tx_miss_retries
            .lock("forced tx miss retries cache")
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
        self.script_misses.clear_many(scripts)
    }

    pub(super) fn extend_script_pubkeys<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (ScriptBuf, (KeychainKind, u32))>,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        // Miss tracking is cleared before cache insertion. Concurrent readers may briefly
        // observe a mismatch between miss-tracking state and cache contents.
        self.clear_script_miss_tracking(entries.iter().map(|(script, _)| script))?;
        #[cfg(test)]
        test_support::pause_at(test_support::HookPoint::BeforeExtendScriptPubkeysInsert);
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

    #[cfg(test)]
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
        let txids: Vec<_> = txids.into_iter().copied().collect();
        self.tx_misses.clear_many(txids.iter())?;
        let mut forced_retries = self.lock_forced_tx_miss_retries()?;
        for txid in &txids {
            forced_retries.remove(txid);
        }
        Ok(())
    }

    fn clear_pending_tx_lookups<'a, I>(&self, txids: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = &'a Txid>,
    {
        let mut pending = self.lock_pending_tx_lookups()?;
        for txid in txids {
            pending.remove(txid);
        }
        Ok(())
    }

    pub(super) fn extend_txs<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (Txid, TransactionDetails)>,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        // Miss tracking is cleared before cache insertion. Concurrent readers may briefly
        // observe a mismatch between miss-tracking state and cache contents.
        self.clear_tx_miss_tracking(entries.iter().map(|(txid, _)| txid))?;
        self.clear_pending_tx_lookups(entries.iter().map(|(txid, _)| txid))?;
        #[cfg(test)]
        test_support::pause_at(test_support::HookPoint::BeforeExtendTxsInsert);
        let mut cache = self.lock_transactions()?;
        cache.extend(entries);
        Ok(())
    }

    pub(super) fn extend_summary_txs<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (Txid, TransactionDetails)>,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        // Miss tracking is cleared before cache insertion. Concurrent readers may briefly
        // observe a mismatch between miss-tracking state and cache contents.
        self.clear_tx_miss_tracking(entries.iter().map(|(txid, _)| txid))?;
        self.clear_pending_tx_lookups(entries.iter().map(|(txid, _)| txid))?;
        #[cfg(test)]
        test_support::pause_at(test_support::HookPoint::BeforeExtendSummaryTxsInsert);
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
        {
            self.lock_forced_tx_miss_retries()?.remove(txid);
        }
        self.record_missing_txid(*txid)?;
        Ok(())
    }

    pub(super) fn script_marked_missing(&self, script: &Script) -> Result<bool, bdk::Error> {
        self.script_misses.is_missing(script)
    }

    pub(super) fn record_missing_script(&self, script: ScriptBuf) -> Result<(), bdk::Error> {
        self.script_misses.record(script)
    }

    pub(super) fn record_and_enqueue_missing_script(
        &self,
        script: ScriptBuf,
    ) -> Result<(), bdk::Error> {
        self.script_misses.record_and_enqueue(script)
    }

    pub(super) fn mark_script_not_missing(&self, script: &Script) -> Result<(), bdk::Error> {
        self.script_misses.clear(script)
    }

    pub(super) fn drain_pending_script_misses(
        &self,
        max: usize,
    ) -> Result<Vec<ScriptBuf>, bdk::Error> {
        self.script_misses.drain_pending(max)
    }

    pub(super) fn requeue_pending_script_misses<I>(&self, scripts: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = ScriptBuf>,
    {
        self.script_misses.requeue_pending(scripts)
    }

    pub(super) fn txid_marked_missing(&self, txid: &Txid) -> Result<bool, bdk::Error> {
        self.tx_misses.is_missing(txid)
    }

    pub(super) fn record_missing_txid(&self, txid: Txid) -> Result<(), bdk::Error> {
        self.tx_misses.record(txid)
    }

    pub(super) fn enqueue_pending_tx_miss(&self, txid: Txid) -> Result<(), bdk::Error> {
        self.tx_misses.enqueue_pending(txid)
    }

    #[cfg(test)]
    pub(super) fn record_and_enqueue_missing_txid(&self, txid: Txid) -> Result<(), bdk::Error> {
        self.tx_misses.record_and_enqueue(txid)
    }

    pub(super) fn pending_tx_miss_queued(&self, txid: &Txid) -> Result<bool, bdk::Error> {
        self.tx_misses.pending_contains(txid)
    }

    #[cfg(test)]
    pub(super) fn forced_tx_miss_retry_recorded(&self, txid: &Txid) -> Result<bool, bdk::Error> {
        let forced_retries = self.lock_forced_tx_miss_retries()?;
        Ok(forced_retries.contains(txid))
    }

    pub(super) fn claim_forced_tx_miss_retry(&self, txid: Txid) -> Result<bool, bdk::Error> {
        let mut forced_retries = self.lock_forced_tx_miss_retries()?;
        Ok(forced_retries.insert(txid))
    }

    pub(super) fn mark_txid_not_missing(&self, txid: &Txid) -> Result<(), bdk::Error> {
        self.tx_misses.clear(txid)?;
        self.lock_pending_tx_lookups()?.remove(txid);
        self.lock_forced_tx_miss_retries()?.remove(txid);
        Ok(())
    }

    pub(super) fn enqueue_pending_tx_lookup(&self, txid: Txid) -> Result<(), bdk::Error> {
        self.lock_pending_tx_lookups()?.insert(txid);
        Ok(())
    }

    pub(super) fn drain_pending_tx_lookups(&self, max: usize) -> Result<Vec<Txid>, bdk::Error> {
        let mut pending = self.lock_pending_tx_lookups()?;
        let drained: Vec<_> = pending.iter().take(max).copied().collect();
        for txid in &drained {
            pending.remove(txid);
        }
        Ok(drained)
    }

    pub(super) fn drain_pending_tx_lookups_including(
        &self,
        required_txid: Txid,
        max: usize,
    ) -> Result<Vec<Txid>, bdk::Error> {
        let mut pending = self.lock_pending_tx_lookups()?;
        let mut drained = Vec::with_capacity(max.max(1));
        drained.push(required_txid);
        pending.remove(&required_txid);

        let additional = max.max(1).saturating_sub(1);
        let rest: Vec<_> = pending.iter().take(additional).copied().collect();
        for txid in &rest {
            pending.remove(txid);
        }
        drained.extend(rest);

        Ok(drained)
    }

    pub(super) fn requeue_pending_tx_lookups<I>(&self, txids: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = Txid>,
    {
        let mut pending = self.lock_pending_tx_lookups()?;
        pending.extend(txids);
        Ok(())
    }

    pub(super) fn drain_pending_tx_misses(&self, max: usize) -> Result<Vec<Txid>, bdk::Error> {
        self.tx_misses.drain_pending(max)
    }

    pub(super) fn drain_pending_tx_misses_including(
        &self,
        required_txid: Txid,
        max: usize,
    ) -> Result<Vec<Txid>, bdk::Error> {
        let mut pending = self.lock_pending_tx_misses()?;
        let mut drained = Vec::with_capacity(max.max(1));
        drained.push(required_txid);
        pending.remove(&required_txid);

        let additional = max.max(1).saturating_sub(1);
        let rest: Vec<_> = pending.iter().take(additional).copied().collect();
        for txid in &rest {
            pending.remove(txid);
        }
        drained.extend(rest);

        Ok(drained)
    }

    pub(super) fn requeue_pending_tx_misses<I>(&self, txids: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = Txid>,
    {
        self.tx_misses.requeue_pending(txids)
    }

    pub(super) fn should_batch_resolve_script_misses(
        &self,
        threshold: usize,
    ) -> Result<bool, bdk::Error> {
        self.script_misses.should_batch_pending(threshold)
    }

    pub(super) fn should_batch_resolve_tx_misses(
        &self,
        threshold: usize,
    ) -> Result<bool, bdk::Error> {
        self.tx_misses.should_batch_pending(threshold)
    }

    pub(super) fn should_batch_resolve_tx_lookups(
        &self,
        threshold: usize,
    ) -> Result<bool, bdk::Error> {
        let pending = self.lock_pending_tx_lookups()?;
        Ok(pending.len() >= threshold)
    }

    pub(super) fn invalidate(&self) {
        self.script_pubkeys.clear_even_if_poisoned();
        self.transactions.clear_even_if_poisoned();
        self.script_misses.clear_even_if_poisoned();
        self.tx_misses.clear_even_if_poisoned();
        self.pending_tx_lookups.clear_even_if_poisoned();
        self.forced_tx_miss_retries.clear_even_if_poisoned();

        self.script_pubkeys_loaded_mask.store(0, Ordering::Release);
        self.raw_txs_fully_loaded.store(false, Ordering::Release);
        self.summary_txs_fully_loaded
            .store(false, Ordering::Release);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::{Condvar, OnceLock};

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub(crate) enum HookPoint {
        BeforeExtendScriptPubkeysInsert,
        BeforeExtendTxsInsert,
        BeforeExtendSummaryTxsInsert,
    }

    #[derive(Default)]
    struct HookState {
        reached: bool,
        released: bool,
    }

    struct InstalledChannels {
        state: Mutex<HookState>,
        signal: Condvar,
    }

    fn hooks() -> &'static Mutex<HashMap<HookPoint, Arc<InstalledChannels>>> {
        static HOOKS: OnceLock<Mutex<HashMap<HookPoint, Arc<InstalledChannels>>>> = OnceLock::new();
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn serial() -> &'static Mutex<()> {
        static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
        SERIAL.get_or_init(|| Mutex::new(()))
    }

    pub(crate) struct InstalledHook {
        point: HookPoint,
        _serial: MutexGuard<'static, ()>,
    }

    impl InstalledHook {
        pub(crate) fn wait_until_reached(&self) {
            let hook = hooks()
                .lock()
                .expect("hooks lock should succeed")
                .get(&self.point)
                .cloned()
                .expect("hook should stay installed while waiting");
            let mut state = hook.state.lock().expect("state lock should succeed");
            while !state.reached {
                state = hook.signal.wait(state).expect("wait should succeed");
            }
        }

        pub(crate) fn release(&mut self) {
            let hook = hooks()
                .lock()
                .expect("hooks lock should succeed")
                .get(&self.point)
                .cloned();
            let Some(hook) = hook else {
                return;
            };

            let mut state = hook.state.lock().expect("state lock should succeed");
            state.released = true;
            hook.signal.notify_all();
        }
    }

    impl Drop for InstalledHook {
        fn drop(&mut self) {
            if let Some(hook) = hooks()
                .lock()
                .expect("hooks lock should succeed")
                .remove(&self.point)
            {
                let mut state = hook.state.lock().expect("state lock should succeed");
                state.released = true;
                hook.signal.notify_all();
            }
        }
    }

    pub(crate) fn install_hook(point: HookPoint) -> InstalledHook {
        let serial = serial().lock().unwrap_or_else(PoisonError::into_inner);
        let hook = Arc::new(InstalledChannels {
            state: Mutex::new(HookState::default()),
            signal: Condvar::new(),
        });

        let previous = hooks()
            .lock()
            .expect("hooks lock should succeed")
            .insert(point, hook);
        assert!(
            previous.is_none(),
            "test hook already installed for {point:?}"
        );

        InstalledHook {
            point,
            _serial: serial,
        }
    }

    pub(super) fn pause_at(point: HookPoint) {
        let hook = hooks()
            .lock()
            .expect("hooks lock should succeed")
            .get(&point)
            .cloned();
        let Some(hook) = hook else {
            return;
        };

        let mut state = hook.state.lock().expect("state lock should succeed");
        if !state.reached {
            state.reached = true;
            hook.signal.notify_all();
        }
        while !state.released {
            state = hook.signal.wait(state).expect("wait should succeed");
        }
    }
}
