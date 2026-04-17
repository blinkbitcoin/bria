use bdk::{
    bitcoin::{blockdata::transaction::OutPoint, Script, ScriptBuf, Transaction, Txid},
    database::{BatchDatabase, BatchOperations, Database, SyncTime},
    KeychainKind, LocalUtxo, TransactionDetails,
};

use super::{
    convert::BdkKeychainKind, lookups::TxLookupMode, ScriptPubkeys, SqlxWalletDb, Transactions,
    Utxos, WalletBatchState,
};

impl BatchOperations for SqlxWalletDb {
    #[tracing::instrument(name = "bdk.batch.set_script_pubkey", skip_all, err)]
    fn set_script_pubkey(
        &mut self,
        script: &Script,
        keychain: KeychainKind,
        path: u32,
    ) -> Result<(), bdk::Error> {
        self.batch.addresses.insert(script.into(), (keychain, path));
        self.cache.mark_script_not_missing(script)?;
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
        self.cache.mark_txid_not_missing(&tx.txid)?;
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

        self.batch.txs.remove(tx_id);

        if deleted.is_some() {
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

        Self::cache_loaded_script_pubkeys(&self.cache, keychain, scripts_with_paths)
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
        let raw_loaded = self.cache.raw_txs_fully_loaded();
        let summary_loaded = self.cache.summary_txs_fully_loaded();
        let txs = if include_raw {
            if raw_loaded {
                self.cache
                    .all_txs()?
                    .into_iter()
                    .map(|tx| (tx.txid, tx))
                    .collect()
            } else {
                let loaded = self
                    .ctx
                    .rt
                    .block_on(async { self.transactions_repo().load_all().await })?;
                self.cache
                    .extend_txs(loaded.iter().map(|(txid, tx)| (*txid, tx.clone())))?;
                self.cache.set_raw_txs_fully_loaded();
                loaded
            }
        } else if raw_loaded || summary_loaded {
            // Once raw txs are fully loaded for this process, serve summary calls from cache to
            // avoid repeated full-table reads. This returns the in-process snapshot (kept current
            // by set/del/commit paths) rather than forcing a fresh DB roundtrip.
            self.cache.all_summary_txs()?
        } else {
            let txs = self
                .ctx
                .rt
                .block_on(async { self.transactions_repo().load_all_summaries().await })?;
            self.cache
                .extend_summary_txs(txs.iter().map(|(txid, tx)| (*txid, tx.clone())))?;
            self.cache.set_summary_txs_fully_loaded();
            txs
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

    #[tracing::instrument(
        name = "bdk.db.get_path_from_script_pubkey",
        skip_all,
        err,
        fields(source)
    )]
    fn get_path_from_script_pubkey(
        &self,
        script: &Script,
    ) -> Result<Option<(KeychainKind, u32)>, bdk::Error> {
        let (result, source) = self.lookup_script_pubkey_path(script)?;
        tracing::Span::current().record("source", tracing::field::display(source));
        Ok(result)
    }

    #[tracing::instrument(name = "bdk.db.get_utxo", skip_all, err)]
    fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<LocalUtxo>, bdk::Error> {
        self.ctx
            .rt
            .block_on(async { self.utxos_repo().find(outpoint).await })
    }

    #[tracing::instrument(name = "bdk.db.get_raw_tx", skip_all, err, fields(source))]
    fn get_raw_tx(&self, tx_id: &Txid) -> Result<Option<Transaction>, bdk::Error> {
        let (tx, source) = self.lookup_tx_with_mode(tx_id, TxLookupMode::RequireRaw)?;
        tracing::Span::current().record("source", tracing::field::display(source));
        Ok(tx.and_then(|tx| tx.transaction))
    }

    #[tracing::instrument(
        name = "bdk.db.get_tx",
        skip_all,
        err,
        fields(source, include_raw = include_raw)
    )]
    fn get_tx(
        &self,
        tx_id: &Txid,
        include_raw: bool,
    ) -> Result<Option<TransactionDetails>, bdk::Error> {
        let (tx, source) = self.lookup_tx(tx_id)?;
        tracing::Span::current().record("source", tracing::field::display(source));
        Ok(tx.map(|tx| {
            if include_raw {
                tx
            } else {
                Self::summary_tx_from_owned(tx)
            }
        }))
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
            miss_resolution: self.miss_resolution,
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

        let mut cache_degraded = false;
        if let Err(error) = self.cache.extend_script_pubkeys(addresses_for_cache) {
            tracing::warn!(
                phase = "script_pubkeys",
                error = %error,
                "cache update failed after successful commit; invalidating cache"
            );
            cache_degraded = true;
        }
        if let Err(error) = self.cache.extend_txs(txs_for_cache) {
            tracing::warn!(
                phase = "txs",
                error = %error,
                "cache update failed after successful commit; invalidating cache"
            );
            cache_degraded = true;
        }
        if cache_degraded {
            self.cache.invalidate();
        }

        Ok(())
    }
}
