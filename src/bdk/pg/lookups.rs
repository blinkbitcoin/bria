use bdk::{
    bitcoin::{Script, Txid},
    KeychainKind, TransactionDetails,
};
use std::collections::HashMap;

use super::SqlxWalletDb;

#[derive(Copy, Clone, Eq, PartialEq)]
pub(super) enum TxLookupMode {
    Any,
    RequireRaw,
}

#[derive(Copy, Clone)]
pub(super) struct MissResolutionPolicy {
    pub threshold: usize,
    pub batch_size: usize,
}

impl Default for MissResolutionPolicy {
    fn default() -> Self {
        Self {
            threshold: 64,
            batch_size: 512,
        }
    }
}

impl SqlxWalletDb {
    fn resolve_pending_script_misses(&self) -> Result<(), bdk::Error> {
        if !self
            .cache
            .should_batch_resolve_script_misses(self.miss_resolution.threshold)?
        {
            return Ok(());
        }

        let pending = self
            .cache
            .drain_pending_script_misses(self.miss_resolution.batch_size)?;
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

        Ok(())
    }

    fn resolve_pending_tx_misses(&self) -> Result<(), bdk::Error> {
        if !self
            .cache
            .should_batch_resolve_tx_misses(self.miss_resolution.threshold)?
        {
            return Ok(());
        }

        let pending = self
            .cache
            .drain_pending_tx_misses(self.miss_resolution.batch_size)?;
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

        Ok(())
    }

    pub(super) fn lookup_script_pubkey_path(
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

    pub(super) fn lookup_tx_with_mode(
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
            Ok((found, "db_hit"))
        } else {
            self.cache.mark_txid_missing(*txid)?;
            Ok((None, "db_miss"))
        }
    }

    pub(super) fn lookup_tx(
        &self,
        txid: &Txid,
    ) -> Result<(Option<TransactionDetails>, &'static str), bdk::Error> {
        self.lookup_tx_with_mode(txid, TxLookupMode::Any)
    }

    pub(super) fn tx_matches_lookup_mode(tx: &TransactionDetails, mode: TxLookupMode) -> bool {
        mode == TxLookupMode::Any || tx.transaction.is_some()
    }

    pub(super) fn summary_tx_from_ref(tx: &TransactionDetails) -> TransactionDetails {
        TransactionDetails {
            transaction: None,
            txid: tx.txid,
            received: tx.received,
            sent: tx.sent,
            fee: tx.fee,
            confirmation_time: tx.confirmation_time.clone(),
        }
    }

    pub(super) fn summary_tx_from_owned(tx: TransactionDetails) -> TransactionDetails {
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

    pub(super) fn overlay_batch_txs(
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
