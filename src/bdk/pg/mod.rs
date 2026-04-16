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
    sync::{Arc, Mutex, MutexGuard},
};
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

#[derive(Clone)]
struct WalletCache {
    script_pubkeys: Arc<Mutex<ScriptPubkeyCache>>,
    transactions: Arc<Mutex<TransactionCache>>,
}

impl WalletCache {
    fn new() -> Self {
        Self {
            script_pubkeys: Arc::new(Mutex::new(HashMap::new())),
            transactions: Arc::new(Mutex::new(HashMap::new())),
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

    fn lookup_tx(&self, txid: &Txid) -> Result<Option<TransactionDetails>, bdk::Error> {
        if let Some(tx) = self.batch.txs.get(txid) {
            return Ok(Some(tx.clone()));
        }

        if let Some(tx) = self.cache.get_tx(txid)? {
            return Ok(Some(tx));
        }

        let found = self
            .ctx
            .rt
            .block_on(async { self.transactions_repo().find_by_id(txid).await })?;

        if let Some(tx) = &found {
            self.cache.insert_tx(tx.txid, tx.clone())?;
        }

        Ok(found)
    }
}

impl BatchOperations for SqlxWalletDb {
    fn set_script_pubkey(
        &mut self,
        script: &Script,
        keychain: KeychainKind,
        path: u32,
    ) -> Result<(), bdk::Error> {
        self.batch.addresses.insert(script.into(), (keychain, path));
        Ok(())
    }

    fn set_utxo(&mut self, utxo: &LocalUtxo) -> Result<(), bdk::Error> {
        self.batch.utxos.push(utxo.clone());
        Ok(())
    }

    fn set_raw_tx(&mut self, _: &Transaction) -> Result<(), bdk::Error> {
        unimplemented!()
    }

    fn set_tx(&mut self, tx: &TransactionDetails) -> Result<(), bdk::Error> {
        self.batch.txs.insert(tx.txid, tx.clone());
        Ok(())
    }

    fn set_last_index(&mut self, kind: KeychainKind, idx: u32) -> Result<(), bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.indexes_repo().persist_last_index(kind, idx).await })
    }

    fn set_sync_time(&mut self, time: SyncTime) -> Result<(), bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.sync_times_repo().persist(time).await })
    }

    fn del_script_pubkey_from_path(
        &mut self,
        _: KeychainKind,
        _: u32,
    ) -> Result<Option<ScriptBuf>, bdk::Error> {
        unimplemented!()
    }
    fn del_path_from_script_pubkey(
        &mut self,
        _: &Script,
    ) -> Result<Option<(KeychainKind, u32)>, bdk::Error> {
        unimplemented!()
    }
    fn del_utxo(&mut self, outpoint: &OutPoint) -> Result<Option<LocalUtxo>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.utxos_repo().delete(outpoint).await })
    }
    fn del_raw_tx(&mut self, _: &Txid) -> Result<Option<Transaction>, bdk::Error> {
        unimplemented!()
    }

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
    fn del_last_index(&mut self, _: KeychainKind) -> Result<std::option::Option<u32>, bdk::Error> {
        unimplemented!()
    }
    fn del_sync_time(&mut self) -> Result<Option<SyncTime>, bdk::Error> {
        unimplemented!()
    }
}

impl Database for SqlxWalletDb {
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
    fn iter_script_pubkeys(
        &self,
        keychain: Option<KeychainKind>,
    ) -> Result<Vec<ScriptBuf>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.script_pubkeys_repo().list_scripts(keychain).await })
    }
    fn iter_utxos(&self) -> Result<Vec<LocalUtxo>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.utxos_repo().list_local_utxos().await })
    }
    fn iter_raw_txs(&self) -> Result<Vec<Transaction>, bdk::Error> {
        unimplemented!()
    }

    fn iter_txs(&self, _: bool) -> Result<Vec<TransactionDetails>, bdk::Error> {
        let mut txs = self
            .ctx
            .rt
            .block_on(async { self.transactions_repo().load_all().await })?;
        txs.extend(self.batch.txs.iter().map(|(id, tx)| (*id, tx.clone())));
        Ok(txs.into_values().collect())
    }

    fn get_script_pubkey_from_path(
        &self,
        keychain: KeychainKind,
        path: u32,
    ) -> Result<Option<ScriptBuf>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.script_pubkeys_repo().find_script(keychain, path).await })
    }
    fn get_path_from_script_pubkey(
        &self,
        script: &Script,
    ) -> Result<Option<(KeychainKind, u32)>, bdk::Error> {
        self.lookup_script_pubkey_path(script)
    }
    fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<LocalUtxo>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.utxos_repo().find(outpoint).await })
    }
    fn get_raw_tx(&self, tx_id: &Txid) -> Result<Option<Transaction>, bdk::Error> {
        self.lookup_tx(tx_id)
            .map(|tx| tx.and_then(|tx| tx.transaction))
    }
    fn get_tx(
        &self,
        tx_id: &Txid,
        _include_raw: bool,
    ) -> Result<Option<TransactionDetails>, bdk::Error> {
        self.lookup_tx(tx_id)
    }
    fn get_last_index(&self, kind: KeychainKind) -> Result<std::option::Option<u32>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.indexes_repo().get_latest(kind).await })
    }
    fn get_sync_time(&self) -> Result<Option<SyncTime>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.sync_times_repo().get().await })
    }
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
        let addresses_for_cache: Vec<_> = batch
            .batch
            .addresses
            .iter()
            .map(|(script, (keychain, path))| (script.clone(), (*keychain, *path)))
            .collect();
        let addresses_for_db: Vec<_> = batch
            .batch
            .addresses
            .drain()
            .map(|(script, (keychain, path))| (BdkKeychainKind::from(keychain), path, script))
            .collect();

        let txs_for_cache: Vec<_> = batch
            .batch
            .txs
            .iter()
            .map(|(txid, tx)| (*txid, tx.clone()))
            .collect();
        let txs_for_db: Vec<_> = batch.batch.txs.drain().map(|(_, tx)| tx).collect();

        let utxos_for_db = std::mem::take(&mut batch.batch.utxos);
        let keychain_id = batch.ctx.keychain_id;
        let pool = batch.ctx.pool.clone();

        batch.ctx.rt.block_on(async move {
            if !addresses_for_db.is_empty() {
                ScriptPubkeys::new(keychain_id, pool.clone())
                    .persist_all(addresses_for_db)
                    .await?;
            }

            if !utxos_for_db.is_empty() {
                Utxos::new(keychain_id, pool.clone())
                    .persist_all(utxos_for_db)
                    .await?;
            }

            if !txs_for_db.is_empty() {
                Transactions::new(keychain_id, pool)
                    .persist_all(txs_for_db)
                    .await?;
            }

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
}
