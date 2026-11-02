use bdk::{
    bitcoin::{blockdata::transaction::OutPoint, Script, ScriptBuf, Transaction, Txid},
    database::{BatchDatabase, BatchOperations, Database, SyncTime},
    KeychainKind, LocalUtxo, TransactionDetails,
};

use std::{sync::Arc, time::Instant};

use tracing::{field, Instrument};

use super::{
    commit_watch::{chunks_for, spawn_watchdog, CommitIds, CommitPhase, CommitProgress},
    convert::BdkKeychainKind,
    lookups::TxLookupMode,
    CommitStats, ScriptPubkeys, SqlxWalletDb, Transactions, Utxos, WalletBatchState,
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
        let (tx, source) = if include_raw {
            self.lookup_tx_with_mode(tx_id, TxLookupMode::RequireRaw)?
        } else {
            self.lookup_tx(tx_id)?
        };
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

#[cfg(test)]
mod tests {
    use super::super::commit_watch::test_capture::{Capture, CapturedSpan};
    use super::*;
    use bdk::bitcoin::hashes::Hash;
    use sqlx::postgres::PgPoolOptions;

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

    #[tokio::test]
    async fn get_tx_include_raw_does_not_return_summary_only_cache_entry() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/test")
            .expect("lazy pool should be created");
        let db = SqlxWalletDb::new(pool, uuid::Uuid::nil().into());

        let txid = Txid::all_zeros();
        db.cache
            .extend_txs([(txid, tx_details(txid))])
            .expect("insert should succeed");
        db.cache.set_raw_txs_fully_loaded();

        let tx = Database::get_tx(&db, &txid, true).expect("get_tx should succeed");
        assert!(tx.is_none());

        let summary = Database::get_tx(&db, &txid, false).expect("get_tx should succeed");
        assert_eq!(summary.map(|s| s.txid), Some(txid));
    }

    struct CommitRun {
        capture: Capture,
        rt: tokio::runtime::Runtime,
        pool: sqlx::PgPool,
        keychain_id: crate::primitives::KeychainId,
        result: Result<(), bdk::Error>,
        stats: CommitStats,
    }

    impl CommitRun {
        fn rows(&self, table: &str) -> i64 {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE keychain_id = $1");
            self.rt
                .block_on(
                    sqlx::query_scalar::<_, i64>(&sql)
                        .bind(uuid::Uuid::from(self.keychain_id))
                        .fetch_one(&self.pool),
                )
                .expect("count should succeed")
        }
    }

    fn pg_url() -> String {
        let pg_host = std::env::var("PG_HOST").unwrap_or("localhost".to_string());
        format!("postgres://user:password@{pg_host}:5432/pg")
    }

    // bdk tables have no foreign keys, so a fresh keychain id isolates each run.
    fn commit_with_capture(stage: impl FnOnce(&mut SqlxWalletDb)) -> CommitRun {
        let capture = Capture::new();
        let rt = capture.runtime();
        let keychain_id = uuid::Uuid::new_v4().into();
        let (pool, result, stats) =
            tracing::dispatcher::with_default(&capture.dispatch.clone(), || {
                let pool = rt
                    .block_on(sqlx::PgPool::connect(&pg_url()))
                    .expect("connect should succeed");
                let mut db = {
                    let _enter = rt.enter();
                    SqlxWalletDb::new(pool.clone(), keychain_id)
                        .with_wallet_id(uuid::Uuid::new_v4().into())
                };
                let mut batch = db.begin_batch();
                stage(&mut batch);
                let result = db.commit_batch(batch);
                (pool, result, db.commit_stats())
            });
        CommitRun {
            capture,
            rt,
            pool,
            keychain_id,
            result,
            stats,
        }
    }

    fn sample_utxo(n: u8) -> LocalUtxo {
        LocalUtxo {
            outpoint: OutPoint::new(Txid::from_slice(&[n; 32]).expect("valid txid"), 0),
            txout: bdk::bitcoin::TxOut {
                value: 1_000,
                script_pubkey: ScriptBuf::from(vec![0x51]),
            },
            keychain: KeychainKind::External,
            is_spent: false,
        }
    }

    fn stage_rows(db: &mut SqlxWalletDb, n: u8) {
        for i in 0..n {
            let script = ScriptBuf::from(vec![0x51, i]);
            db.set_script_pubkey(&script, KeychainKind::External, u32::from(i))
                .expect("stage script");
            db.set_utxo(&sample_utxo(i + 1)).expect("stage utxo");
            db.set_tx(&tx_details(
                Txid::from_slice(&[i + 101; 32]).expect("valid txid"),
            ))
            .expect("stage tx");
        }
    }

    fn field_u64(span: &CapturedSpan, name: &str) -> u64 {
        span.field(name)
            .unwrap_or_else(|| panic!("{name} not recorded"))
            .parse()
            .expect("numeric field")
    }

    #[test]
    fn commit_batch_emits_commit_and_chunk_spans() {
        let run = commit_with_capture(|db| stage_rows(db, 2));
        run.result.as_ref().expect("commit should succeed");

        let commits = run.capture.named("bdk.commit_batch");
        assert_eq!(commits.len(), 1);
        let commit = &commits[0];
        assert_eq!(
            commit.field("keychain_id"),
            Some(run.keychain_id.to_string().as_str())
        );
        assert!(commit.field("wallet_id").is_some());
        assert!(commit.field("backend_pid").is_some());
        assert_eq!(commit.field("n_addresses"), Some("2"));
        assert_eq!(commit.field("n_utxos"), Some("2"));
        assert_eq!(commit.field("n_txs"), Some("2"));
        assert_eq!(commit.field("chunks_total"), Some("3"));
        let bytes_total = field_u64(commit, "bytes_total");
        assert!(bytes_total > 0);
        assert!(!commit.has_error_event());

        let chunks = run.capture.named("bdk.persist_chunk");
        let tables: Vec<_> = chunks.iter().map(|c| c.field("table").unwrap()).collect();
        assert_eq!(
            tables,
            vec!["bdk_script_pubkeys", "bdk_utxos", "bdk_transactions"]
        );
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.parent, Some("bdk.commit_batch"));
            assert_eq!(chunk.field("rows"), Some("2"));
            assert_eq!(
                chunk.field("chunk_index"),
                Some((i + 1).to_string().as_str())
            );
            assert!(!chunk.has_error_event());
        }
        let chunk_bytes: u64 = chunks.iter().map(|c| field_u64(c, "bytes")).sum();
        assert_eq!(chunk_bytes, bytes_total);

        let commit_spans = run.capture.named("bdk.commit_batch.commit");
        assert_eq!(commit_spans.len(), 1);
        assert_eq!(commit_spans[0].parent, Some("bdk.commit_batch"));
        assert!(run
            .capture
            .named("bdk.commit_batch.still_running")
            .is_empty());

        assert_eq!(
            run.stats,
            CommitStats {
                n_addresses: 2,
                n_utxos: 2,
                n_txs: 2,
                bytes: bytes_total,
            }
        );
        assert_eq!(run.rows("bdk_script_pubkeys"), 2);
        assert_eq!(run.rows("bdk_utxos"), 2);
        assert_eq!(run.rows("bdk_transactions"), 2);
    }

    #[test]
    fn commit_batch_with_empty_batch_emits_no_chunk_spans() {
        let run = commit_with_capture(|_| {});
        run.result.as_ref().expect("commit should succeed");

        let commits = run.capture.named("bdk.commit_batch");
        assert_eq!(commits.len(), 1);
        let commit = &commits[0];
        assert_eq!(commit.field("n_addresses"), Some("0"));
        assert_eq!(commit.field("n_utxos"), Some("0"));
        assert_eq!(commit.field("n_txs"), Some("0"));
        assert_eq!(commit.field("chunks_total"), Some("0"));
        assert_eq!(commit.field("bytes_total"), Some("0"));
        assert!(run.capture.named("bdk.persist_chunk").is_empty());
        assert_eq!(run.capture.named("bdk.commit_batch.commit").len(), 1);
        assert!(run
            .capture
            .named("bdk.commit_batch.still_running")
            .is_empty());
        assert_eq!(run.stats, CommitStats::default());
    }

    #[test]
    fn commit_batch_error_marks_chunk_span_and_propagates() {
        let run = commit_with_capture(|db| {
            stage_rows(db, 1);
            // The same outpoint twice in one upsert makes the utxos INSERT fail.
            let mut dup = sample_utxo(1);
            dup.is_spent = true;
            db.set_utxo(&dup).expect("stage duplicate utxo");
        });

        let message = match &run.result {
            Err(bdk::Error::Generic(message)) => message.clone(),
            other => panic!("unexpected result: {other:?}"),
        };
        assert!(
            message.contains("cannot affect row a second time"),
            "{message}"
        );

        let chunks = run.capture.named("bdk.persist_chunk");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].field("table"), Some("bdk_script_pubkeys"));
        assert!(!chunks[0].has_error_event());
        let failed = &chunks[1];
        assert_eq!(failed.field("table"), Some("bdk_utxos"));
        assert_eq!(failed.field("rows"), Some("2"));
        let event = failed
            .events
            .iter()
            .find(|e| e.level == tracing::Level::ERROR)
            .expect("error event in failed chunk");
        assert_eq!(event.fields.get("error"), Some(&message));

        let commits = run.capture.named("bdk.commit_batch");
        assert_eq!(commits.len(), 1);
        // The failure is logged once, on the chunk span only.
        assert!(!commits[0].has_error_event());
        let error_events = failed
            .events
            .iter()
            .filter(|e| e.level == tracing::Level::ERROR)
            .count();
        assert_eq!(error_events, 1);
        assert!(run.capture.named("bdk.commit_batch.commit").is_empty());
        assert!(run
            .capture
            .named("bdk.commit_batch.still_running")
            .is_empty());

        assert_eq!(run.stats, CommitStats::default());
        assert_eq!(run.rows("bdk_script_pubkeys"), 0);
        assert_eq!(run.rows("bdk_utxos"), 0);
        assert_eq!(run.rows("bdk_transactions"), 0);
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
            commit_stats: CommitStats::default(),
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
        let wallet_id = batch.ctx.wallet_id;
        let pool = batch.ctx.pool.clone();

        let n_addresses = addresses_for_db.len();
        let n_utxos = utxos_for_db.len();
        let n_txs = txs_for_db.len();
        let chunks_total = chunks_for(n_addresses, ScriptPubkeys::PERSIST_BATCH_SIZE)
            + chunks_for(n_utxos, Utxos::PERSIST_BATCH_SIZE)
            + chunks_for(n_txs, Transactions::PERSIST_BATCH_SIZE);
        let progress = Arc::new(CommitProgress::new(chunks_total));
        let commit_span = tracing::info_span!(
            "bdk.commit_batch",
            keychain_id = %keychain_id,
            wallet_id = field::Empty,
            n_addresses,
            n_utxos,
            n_txs,
            chunks_total,
            bytes_total = field::Empty,
            backend_pid = field::Empty,
        );
        if let Some(wallet_id) = wallet_id {
            commit_span.record("wallet_id", field::display(wallet_id));
        }

        let result = batch.ctx.rt.block_on(
            {
                let progress = Arc::clone(&progress);
                let commit_span = commit_span.clone();
                async move {
                    let started = Instant::now();
                    let mut tx = pool
                        .begin()
                        .await
                        .map_err(|e| bdk::Error::Generic(e.to_string()))?;

                    let backend_pid = sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
                        .fetch_one(tx.as_mut())
                        .await
                        .map_err(|e| bdk::Error::Generic(e.to_string()))?;
                    commit_span.record("backend_pid", backend_pid);
                    let _watchdog = spawn_watchdog(
                        started,
                        pool.clone(),
                        Some(backend_pid),
                        Arc::clone(&progress),
                        CommitIds {
                            keychain_id,
                            wallet_id,
                        },
                        commit_span.clone(),
                    );

                    if !addresses_for_db.is_empty() {
                        ScriptPubkeys::new(keychain_id, pool.clone())
                            .persist_all_in_tx(&mut tx, addresses_for_db, &progress)
                            .await?;
                    }

                    if !utxos_for_db.is_empty() {
                        Utxos::new(keychain_id, pool.clone())
                            .persist_all_in_tx(&mut tx, utxos_for_db, &progress)
                            .await?;
                    }

                    if !txs_for_db.is_empty() {
                        Transactions::new(keychain_id, pool)
                            .persist_all_in_tx(&mut tx, txs_for_db, &progress)
                            .await?;
                    }

                    progress.set_phase(CommitPhase::Commit);
                    tx.commit()
                        .instrument(tracing::info_span!("bdk.commit_batch.commit"))
                        .await
                        .map_err(|e| bdk::Error::Generic(e.to_string()))?;

                    Ok::<_, bdk::Error>(())
                }
            }
            .instrument(commit_span.clone()),
        );
        let bytes = progress.bytes_sent();
        commit_span.record("bytes_total", bytes);
        drop(commit_span);
        result?;

        self.commit_stats += CommitStats {
            n_addresses: n_addresses as u64,
            n_utxos: n_utxos as u64,
            n_txs: n_txs as u64,
            bytes,
        };

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
