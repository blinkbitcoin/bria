mod convert;
mod descriptor_checksum;
mod index;
mod script_pubkeys;
mod sync_times;
mod transactions;
mod utxos;

use bdk::{
    bitcoin::{blockdata::transaction::OutPoint, Script, ScriptBuf, Transaction, Txid},
    database::{BatchDatabase, BatchOperations, Database, SyncTime},
    KeychainKind, LocalUtxo, TransactionDetails,
};
use sqlx::PgPool;
use tokio::runtime::Handle;

use crate::primitives::*;
use convert::BdkKeychainKind;
use descriptor_checksum::DescriptorChecksums;
use index::Indexes;
use script_pubkeys::ScriptPubkeys;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
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
    // Process-local hint for which keychain script path sets are fully hydrated.
    // Bit 0: external, bit 1: internal.
    script_pubkeys_loaded_mask: Arc<AtomicU8>,
    // Process-local hint: true means this instance has already hydrated raw tx details
    // from the DB at least once. It is intentionally not synchronized across processes.
    raw_txs_fully_loaded: Arc<AtomicBool>,
}

impl WalletCache {
    fn new() -> Self {
        Self {
            script_pubkeys: Arc::new(Mutex::new(HashMap::new())),
            transactions: Arc::new(Mutex::new(HashMap::new())),
            script_pubkeys_loaded_mask: Arc::new(AtomicU8::new(0)),
            raw_txs_fully_loaded: Arc::new(AtomicBool::new(false)),
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
        let mut cache = self.lock_script_pubkeys()?;
        cache.insert(script, path);
        Ok(())
    }

    fn extend_script_pubkeys<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (ScriptBuf, (KeychainKind, u32))>,
    {
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
        let mut cache = self.lock_transactions()?;
        cache.insert(txid, tx);
        Ok(())
    }

    fn extend_txs<I>(&self, entries: I) -> Result<(), bdk::Error>
    where
        I: IntoIterator<Item = (Txid, TransactionDetails)>,
    {
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

    fn raw_txs_fully_loaded(&self) -> bool {
        self.raw_txs_fully_loaded.load(Ordering::Acquire)
    }

    fn set_raw_txs_fully_loaded(&self) {
        self.raw_txs_fully_loaded.store(true, Ordering::Release);
    }

    fn remove_tx(&self, txid: &Txid) -> Result<(), bdk::Error> {
        let mut cache = self.lock_transactions()?;
        cache.remove(txid);
        Ok(())
    }
}

pub struct SqlxWalletDb {
    ctx: WalletDbContext,
    cache: WalletCache,
    batch: WalletBatchState,
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
        }
    }

    fn script_pubkeys_repo(&self) -> ScriptPubkeys {
        ScriptPubkeys::new(self.ctx.keychain_id, self.ctx.pool.clone())
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

    fn lookup_script_pubkey_path(
        &self,
        script: &Script,
    ) -> Result<Option<(KeychainKind, u32)>, bdk::Error> {
        if let Some(path) = self.batch.addresses.get(script) {
            return Ok(Some(*path));
        }

        if let Some(path) = self.cache.get_script_pubkey_path(script)? {
            return Ok(Some(path));
        }

        let script_pubkey = script.to_owned();
        let found = self
            .ctx
            .rt
            .block_on(async { self.script_pubkeys_repo().find_path(&script_pubkey).await })?;

        if let Some((kind, path)) = found {
            let value = (KeychainKind::from(kind), path);
            self.cache.insert_script_pubkey(script_pubkey, value)?;
            return Ok(Some(value));
        }

        Ok(None)
    }

    fn lookup_tx_with_mode(
        &self,
        txid: &Txid,
        mode: TxLookupMode,
    ) -> Result<Option<TransactionDetails>, bdk::Error> {
        if let Some(tx) = self.batch.txs.get(txid) {
            if Self::tx_matches_lookup_mode(tx, mode) {
                return Ok(Some(tx.clone()));
            }

            return Ok(None);
        }

        if let Some(tx) = self.cache.get_tx(txid)? {
            if Self::tx_matches_lookup_mode(&tx, mode) {
                return Ok(Some(tx));
            }

            if self.cache.raw_txs_fully_loaded() {
                return Ok(None);
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
        }

        Ok(found)
    }

    fn lookup_tx(&self, txid: &Txid) -> Result<Option<TransactionDetails>, bdk::Error> {
        self.lookup_tx_with_mode(txid, TxLookupMode::Any)
    }

    fn tx_matches_lookup_mode(tx: &TransactionDetails, mode: TxLookupMode) -> bool {
        mode == TxLookupMode::Any || tx.transaction.is_some()
    }

    fn tx_without_raw(mut tx: TransactionDetails) -> TransactionDetails {
        tx.transaction = None;
        tx
    }

    fn without_raw(tx: TransactionDetails) -> (Txid, TransactionDetails) {
        let txid = tx.txid;
        (txid, Self::tx_without_raw(tx))
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
                    .map(|(id, tx)| (*id, Self::tx_without_raw(tx.clone()))),
            );
        }

        txs
    }
}

impl BatchOperations for SqlxWalletDb {
    #[tracing::instrument(name = "bdk.batch.set_script_pubkey", skip_all, err)]
    fn set_script_pubkey(
        &mut self,
        script: &Script,
        keychain: KeychainKind,
        path: u32,
    ) -> Result<(), bdk::Error> {
        self.batch.addresses.insert(script.into(), (keychain, path));
        Ok(())
    }

    #[tracing::instrument(name = "bdk.batch.set_utxo", skip_all, err)]
    fn set_utxo(&mut self, utxo: &LocalUtxo) -> Result<(), bdk::Error> {
        self.batch.utxos.push(utxo.clone());
        Ok(())
    }

    #[tracing::instrument(name = "bdk.batch.set_raw_tx", skip_all, err)]
    fn set_raw_tx(&mut self, _: &Transaction) -> Result<(), bdk::Error> {
        Err(Self::unsupported_operation("set_raw_tx"))
    }

    #[tracing::instrument(name = "bdk.batch.set_tx", skip_all, err)]
    fn set_tx(&mut self, tx: &TransactionDetails) -> Result<(), bdk::Error> {
        self.batch.txs.insert(tx.txid, tx.clone());
        Ok(())
    }

    #[tracing::instrument(name = "bdk.batch.set_last_index", skip_all, err)]
    fn set_last_index(&mut self, kind: KeychainKind, idx: u32) -> Result<(), bdk::Error> {
        // NOTE: This write is intentionally immediate because BDK may call it outside of
        // `commit_batch` flow.
        self.ctx
            .rt
            .block_on(async { self.indexes_repo().persist_last_index(kind, idx).await })
    }

    #[tracing::instrument(name = "bdk.batch.set_sync_time", skip_all, err)]
    fn set_sync_time(&mut self, time: SyncTime) -> Result<(), bdk::Error> {
        // NOTE: This write is intentionally immediate because BDK may call it outside of
        // `commit_batch` flow.
        self.ctx
            .rt
            .block_on(async { self.sync_times_repo().persist(time).await })
    }

    #[tracing::instrument(name = "bdk.batch.del_script_pubkey_from_path", skip_all, err)]
    fn del_script_pubkey_from_path(
        &mut self,
        _: KeychainKind,
        _: u32,
    ) -> Result<Option<ScriptBuf>, bdk::Error> {
        Err(Self::unsupported_operation("del_script_pubkey_from_path"))
    }

    #[tracing::instrument(name = "bdk.batch.del_path_from_script_pubkey", skip_all, err)]
    fn del_path_from_script_pubkey(
        &mut self,
        _: &Script,
    ) -> Result<Option<(KeychainKind, u32)>, bdk::Error> {
        Err(Self::unsupported_operation("del_path_from_script_pubkey"))
    }

    #[tracing::instrument(name = "bdk.batch.del_utxo", skip_all, err)]
    fn del_utxo(&mut self, outpoint: &OutPoint) -> Result<Option<LocalUtxo>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.utxos_repo().delete(outpoint).await })
    }

    #[tracing::instrument(name = "bdk.batch.del_raw_tx", skip_all, err)]
    fn del_raw_tx(&mut self, _: &Txid) -> Result<Option<Transaction>, bdk::Error> {
        Err(Self::unsupported_operation("del_raw_tx"))
    }

    #[tracing::instrument(name = "bdk.batch.del_tx", skip_all, err)]
    fn del_tx(
        &mut self,
        tx_id: &Txid,
        _include_raw: bool,
    ) -> Result<Option<TransactionDetails>, bdk::Error> {
        let deleted = self
            .ctx
            .rt
            .block_on(async { self.transactions_repo().delete(tx_id).await })?;

        if deleted.is_some() {
            self.batch.txs.remove(tx_id);
            self.cache.remove_tx(tx_id)?;
        }

        Ok(deleted)
    }

    #[tracing::instrument(name = "bdk.batch.del_last_index", skip_all, err)]
    fn del_last_index(&mut self, _: KeychainKind) -> Result<std::option::Option<u32>, bdk::Error> {
        Err(Self::unsupported_operation("del_last_index"))
    }

    #[tracing::instrument(name = "bdk.batch.del_sync_time", skip_all, err)]
    fn del_sync_time(&mut self) -> Result<Option<SyncTime>, bdk::Error> {
        Err(Self::unsupported_operation("del_sync_time"))
    }
}

impl Database for SqlxWalletDb {
    #[tracing::instrument(name = "bdk.db.check_descriptor_checksum", skip_all, err)]
    fn check_descriptor_checksum<B>(
        &mut self,
        keychain: KeychainKind,
        script_bytes: B,
    ) -> Result<(), bdk::Error>
    where
        B: AsRef<[u8]>,
    {
        self.ctx.rt.block_on(async {
            let checksums = self.descriptor_checksums_repo();
            checksums
                .check_or_persist_descriptor_checksum(keychain, script_bytes.as_ref())
                .await?;

            Ok(())
        })
    }

    #[tracing::instrument(name = "bdk.db.iter_script_pubkeys", skip_all, err)]
    fn iter_script_pubkeys(
        &self,
        keychain: Option<KeychainKind>,
    ) -> Result<Vec<ScriptBuf>, bdk::Error> {
        if self.cache.script_pubkeys_fully_loaded(keychain) {
            return self.cache.all_script_pubkeys(keychain);
        }

        let scripts_with_paths = self.ctx.rt.block_on(async {
            self.script_pubkeys_repo()
                .list_scripts_with_paths(keychain)
                .await
        })?;

        self.cache.extend_script_pubkeys(
            scripts_with_paths
                .iter()
                .map(|(script, path)| (script.clone(), *path)),
        )?;
        self.cache.mark_script_pubkeys_loaded(keychain);

        Ok(scripts_with_paths
            .into_iter()
            .map(|(script, _)| script)
            .collect())
    }

    #[tracing::instrument(name = "bdk.db.iter_utxos", skip_all, err)]
    fn iter_utxos(&self) -> Result<Vec<LocalUtxo>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.utxos_repo().list_local_utxos().await })
    }

    #[tracing::instrument(name = "bdk.db.iter_raw_txs", skip_all, err)]
    fn iter_raw_txs(&self) -> Result<Vec<Transaction>, bdk::Error> {
        Err(Self::unsupported_operation("iter_raw_txs"))
    }

    #[tracing::instrument(name = "bdk.db.iter_txs", skip_all, err)]
    fn iter_txs(&self, include_raw: bool) -> Result<Vec<TransactionDetails>, bdk::Error> {
        let txs = match (include_raw, self.cache.raw_txs_fully_loaded()) {
            (true, true) => self
                .cache
                .all_txs()?
                .into_iter()
                .map(|tx| (tx.txid, tx))
                .collect(),
            (true, false) => {
                let loaded = self
                    .ctx
                    .rt
                    .block_on(async { self.transactions_repo().load_all().await })?;
                self.cache
                    .extend_txs(loaded.iter().map(|(txid, tx)| (*txid, tx.clone())))?;
                self.cache.set_raw_txs_fully_loaded();
                loaded
            }
            (false, true) => self
                .cache
                .all_txs()?
                .into_iter()
                .map(Self::without_raw)
                .collect(),
            (false, false) => {
                let txs = self
                    .ctx
                    .rt
                    .block_on(async { self.transactions_repo().load_all_summaries().await })?;
                self.cache
                    .extend_summary_txs(txs.iter().map(|(txid, tx)| (*txid, tx.clone())))?;
                txs
            }
        };

        Ok(Self::overlay_batch_txs(txs, &self.batch.txs, include_raw)
            .into_values()
            .collect())
    }

    #[tracing::instrument(name = "bdk.db.get_script_pubkey_from_path", skip_all, err)]
    fn get_script_pubkey_from_path(
        &self,
        keychain: KeychainKind,
        path: u32,
    ) -> Result<Option<ScriptBuf>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.script_pubkeys_repo().find_script(keychain, path).await })
    }

    #[tracing::instrument(name = "bdk.db.get_path_from_script_pubkey", skip_all, err)]
    fn get_path_from_script_pubkey(
        &self,
        script: &Script,
    ) -> Result<Option<(KeychainKind, u32)>, bdk::Error> {
        self.lookup_script_pubkey_path(script)
    }

    #[tracing::instrument(name = "bdk.db.get_utxo", skip_all, err)]
    fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<LocalUtxo>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.utxos_repo().find(outpoint).await })
    }

    #[tracing::instrument(name = "bdk.db.get_raw_tx", skip_all, err)]
    fn get_raw_tx(&self, tx_id: &Txid) -> Result<Option<Transaction>, bdk::Error> {
        self.lookup_tx_with_mode(tx_id, TxLookupMode::RequireRaw)
            .map(|tx| tx.and_then(|tx| tx.transaction))
    }

    #[tracing::instrument(name = "bdk.db.get_tx", skip_all, err)]
    fn get_tx(
        &self,
        tx_id: &Txid,
        include_raw: bool,
    ) -> Result<Option<TransactionDetails>, bdk::Error> {
        self.lookup_tx(tx_id).map(|tx| {
            tx.map(|mut tx| {
                if !include_raw {
                    tx.transaction = None;
                }
                tx
            })
        })
    }

    #[tracing::instrument(name = "bdk.db.get_last_index", skip_all, err)]
    fn get_last_index(&self, kind: KeychainKind) -> Result<std::option::Option<u32>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.indexes_repo().get_latest(kind).await })
    }

    #[tracing::instrument(name = "bdk.db.get_sync_time", skip_all, err)]
    fn get_sync_time(&self) -> Result<Option<SyncTime>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.sync_times_repo().get().await })
    }

    #[tracing::instrument(name = "bdk.db.increment_last_index", skip_all, err)]
    fn increment_last_index(&mut self, keychain: KeychainKind) -> Result<u32, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.indexes_repo().increment(keychain).await })
    }
}

impl BatchDatabase for SqlxWalletDb {
    type Batch = Self;

    fn begin_batch(&self) -> <Self as BatchDatabase>::Batch {
        SqlxWalletDb {
            ctx: self.ctx.clone(),
            cache: self.cache.clone(),
            batch: WalletBatchState::default(),
        }
    }

    fn commit_batch(
        &mut self,
        mut batch: <Self as BatchDatabase>::Batch,
    ) -> Result<(), bdk::Error> {
        // Atomic scope here is limited to staged script pubkeys, utxos, and transactions.
        // `set_last_index` / `set_sync_time` remain immediate writes by design.
        let (addresses_for_cache, addresses_for_db): (Vec<_>, Vec<_>) = batch
            .batch
            .addresses
            .drain()
            .map(|(script, (keychain, path))| {
                let cache_entry = (script.clone(), (keychain, path));
                let db_entry = (BdkKeychainKind::from(keychain), path, script);
                (cache_entry, db_entry)
            })
            .unzip();

        let (txs_for_cache, txs_for_db): (Vec<_>, Vec<_>) = batch
            .batch
            .txs
            .drain()
            .map(|(txid, tx)| ((txid, tx.clone()), tx))
            .unzip();

        let utxos_for_db = std::mem::take(&mut batch.batch.utxos);
        let keychain_id = batch.ctx.keychain_id;
        let pool = batch.ctx.pool.clone();

        batch.ctx.rt.block_on(async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| bdk::Error::Generic(e.to_string()))?;

            if !addresses_for_db.is_empty() {
                ScriptPubkeys::new(keychain_id, pool.clone())
                    .persist_all_in_tx(&mut tx, addresses_for_db)
                    .await?;
            }

            if !utxos_for_db.is_empty() {
                Utxos::new(keychain_id, pool.clone())
                    .persist_all_in_tx(&mut tx, utxos_for_db)
                    .await?;
            }

            if !txs_for_db.is_empty() {
                Transactions::new(keychain_id, pool)
                    .persist_all_in_tx(&mut tx, txs_for_db)
                    .await?;
            }

            tx.commit()
                .await
                .map_err(|e| bdk::Error::Generic(e.to_string()))?;

            Ok::<_, bdk::Error>(())
        })?;

        self.cache.extend_script_pubkeys(addresses_for_cache)?;
        self.cache.extend_txs(txs_for_cache)?;
        Ok(())
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

        cache.set_raw_txs_fully_loaded();
        assert!(cache.raw_txs_fully_loaded());
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
}
