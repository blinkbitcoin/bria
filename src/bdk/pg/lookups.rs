use bdk::{
    bitcoin::{Script, Txid},
    KeychainKind, TransactionDetails,
};
use std::collections::HashMap;

use super::SqlxWalletDb;

type LookupSource = &'static str;
type ScriptPathLookup = (Option<(KeychainKind, u32)>, LookupSource);
type TxLookup = (Option<TransactionDetails>, LookupSource);

enum ForcedTxLookupOutcome {
    NotQueried,
    NotFound,
    FoundModeMatch(TransactionDetails),
    FoundModeMiss,
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub(super) enum TxLookupMode {
    Any,
    RequireRaw,
}

#[derive(Copy, Clone)]
pub(super) struct MissResolutionPolicy {
    pub tx_lookup_threshold: usize,
    pub tx_lookup_batch_size: usize,
    pub tx_miss_threshold: usize,
    pub tx_miss_batch_size: usize,
    pub script_miss_threshold: usize,
    pub script_miss_batch_size: usize,
}

impl Default for MissResolutionPolicy {
    fn default() -> Self {
        Self {
            tx_lookup_threshold: 64,
            tx_lookup_batch_size: 512,
            tx_miss_threshold: 64,
            tx_miss_batch_size: 512,
            script_miss_threshold: 64,
            script_miss_batch_size: 512,
        }
    }
}

impl SqlxWalletDb {
    fn unresolved_txids(
        pending: Vec<Txid>,
        found: &HashMap<Txid, TransactionDetails>,
    ) -> Vec<Txid> {
        pending
            .into_iter()
            .filter(|txid| !found.contains_key(txid))
            .collect()
    }

    fn lookup_cached_tx_with_mode(
        &self,
        txid: &Txid,
        mode: TxLookupMode,
        hit_source: LookupSource,
        mode_miss_source: LookupSource,
    ) -> Result<Option<TxLookup>, bdk::Error> {
        let Some(tx) = self.cache.get_tx(txid)? else {
            return Ok(None);
        };

        if Self::tx_matches_lookup_mode(&tx, mode) {
            return Ok(Some((Some(tx), hit_source)));
        }

        if self.cache.raw_txs_fully_loaded() {
            self.cache.record_missing_txid(*txid)?;
            return Ok(Some((None, mode_miss_source)));
        }

        Ok(None)
    }

    fn resolve_pending_script_misses(&self) -> Result<(), bdk::Error> {
        if !self
            .cache
            .should_batch_resolve_script_misses(self.miss_resolution.script_miss_threshold)?
        {
            return Ok(());
        }

        let pending = self
            .cache
            .drain_pending_script_misses(self.miss_resolution.script_miss_batch_size)?;
        if pending.is_empty() {
            return Ok(());
        }

        let found = match self.ctx.rt.block_on(async {
            self.script_pubkeys_repo()
                .find_paths_for_scripts(&pending)
                .await
        }) {
            Ok(found) => found,
            Err(error) => {
                self.cache.requeue_pending_script_misses(pending)?;
                return Err(error);
            }
        };

        if !found.is_empty() {
            self.cache.extend_script_pubkeys(
                found
                    .into_iter()
                    .map(|(script, (kind, path))| (script, (KeychainKind::from(kind), path))),
            )?;
        }

        Ok(())
    }

    fn resolve_pending_tx_misses_internal(
        &self,
        force_txid: Option<Txid>,
    ) -> Result<bool, bdk::Error> {
        if force_txid.is_none()
            && !self
                .cache
                .should_batch_resolve_tx_misses(self.miss_resolution.tx_miss_threshold)?
        {
            return Ok(false);
        }

        let pending = if let Some(txid) = force_txid {
            self.cache
                .drain_pending_tx_misses_including(txid, self.miss_resolution.tx_miss_batch_size)?
        } else {
            self.cache
                .drain_pending_tx_misses(self.miss_resolution.tx_miss_batch_size)?
        };
        if pending.is_empty() {
            return Ok(false);
        }

        let found = match self
            .ctx
            .rt
            .block_on(async { self.transactions_repo().find_by_ids(&pending).await })
        {
            Ok(found) => found,
            Err(error) => {
                self.cache.requeue_pending_tx_misses(pending)?;
                return Err(error);
            }
        };

        let unresolved = Self::unresolved_txids(pending, &found);

        if !found.is_empty() {
            self.cache.extend_txs(found)?;
        }

        if !unresolved.is_empty() {
            self.cache.requeue_pending_tx_misses(unresolved)?;
        }

        Ok(true)
    }

    fn resolve_pending_tx_misses(&self) -> Result<(), bdk::Error> {
        let _ = self.resolve_pending_tx_misses_internal(None)?;
        Ok(())
    }

    fn resolve_pending_tx_misses_force(&self, txid: Txid) -> Result<bool, bdk::Error> {
        self.resolve_pending_tx_misses_internal(Some(txid))
    }

    fn resolve_pending_tx_lookups_internal(
        &self,
        force_txid: Option<Txid>,
        mode: TxLookupMode,
    ) -> Result<ForcedTxLookupOutcome, bdk::Error> {
        if force_txid.is_none()
            && !self
                .cache
                .should_batch_resolve_tx_lookups(self.miss_resolution.tx_lookup_threshold)?
        {
            return Ok(ForcedTxLookupOutcome::NotQueried);
        }

        let pending = if let Some(txid) = force_txid {
            self.cache.drain_pending_tx_lookups_including(
                txid,
                self.miss_resolution.tx_lookup_batch_size,
            )?
        } else {
            self.cache
                .drain_pending_tx_lookups(self.miss_resolution.tx_lookup_batch_size)?
        };
        if pending.is_empty() {
            return Ok(ForcedTxLookupOutcome::NotQueried);
        }

        let found = match self
            .ctx
            .rt
            .block_on(async { self.transactions_repo().find_by_ids(&pending).await })
        {
            Ok(found) => found,
            Err(error) => {
                self.cache.requeue_pending_tx_lookups(pending)?;
                return Err(error);
            }
        };

        let forced_outcome = force_txid.map_or(ForcedTxLookupOutcome::NotQueried, |txid| {
            found
                .get(&txid)
                .cloned()
                .map_or(ForcedTxLookupOutcome::NotFound, |tx| {
                    if Self::tx_matches_lookup_mode(&tx, mode) {
                        ForcedTxLookupOutcome::FoundModeMatch(tx)
                    } else {
                        ForcedTxLookupOutcome::FoundModeMiss
                    }
                })
        });

        if !found.is_empty() {
            self.cache.extend_txs(found)?;
        }

        Ok(forced_outcome)
    }

    fn resolve_pending_tx_lookups(&self) -> Result<(), bdk::Error> {
        let _ = self.resolve_pending_tx_lookups_internal(None, TxLookupMode::Any)?;
        Ok(())
    }

    fn resolve_pending_tx_lookups_force(
        &self,
        txid: Txid,
        mode: TxLookupMode,
    ) -> Result<ForcedTxLookupOutcome, bdk::Error> {
        self.resolve_pending_tx_lookups_internal(Some(txid), mode)
    }

    pub(super) fn lookup_script_pubkey_path(
        &self,
        script: &Script,
    ) -> Result<ScriptPathLookup, bdk::Error> {
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
            self.cache.record_missing_script(script.to_owned())?;
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

        self.cache
            .record_and_enqueue_missing_script(script_pubkey)?;

        Ok((None, "db_miss"))
    }

    pub(super) fn lookup_tx_with_mode(
        &self,
        txid: &Txid,
        mode: TxLookupMode,
    ) -> Result<TxLookup, bdk::Error> {
        if let Some(tx) = self.batch.txs.get(txid) {
            if Self::tx_matches_lookup_mode(tx, mode) {
                return Ok((Some(tx.clone()), "batch"));
            }

            return Ok((None, "batch_mode_miss"));
        }

        if let Some(result) =
            self.lookup_cached_tx_with_mode(txid, mode, "cache", "cache_mode_miss")?
        {
            return Ok(result);
        }

        if self.cache.txid_marked_missing(txid)? {
            let should_force_miss_retry = if self.cache.pending_tx_miss_queued(txid)? {
                self.cache.claim_forced_tx_miss_retry(*txid)?
            } else {
                false
            };
            if should_force_miss_retry && self.resolve_pending_tx_misses_force(*txid)? {
                if let Some(result) = self.lookup_cached_tx_with_mode(
                    txid,
                    mode,
                    "forced_miss_resolve",
                    "forced_miss_resolve_mode_miss",
                )? {
                    return Ok(result);
                }
            }

            tracing::trace!("tx miss cache hit");
            return Ok((None, "miss_cache"));
        }

        // Once raw txs are fully loaded in this process, a miss is definitive.
        if self.cache.raw_txs_fully_loaded() {
            self.cache.record_missing_txid(*txid)?;
            return Ok((None, "fully_loaded_miss"));
        }

        self.cache.enqueue_pending_tx_lookup(*txid)?;
        self.resolve_pending_tx_lookups()?;
        if let Some(result) =
            self.lookup_cached_tx_with_mode(txid, mode, "batch_lookup", "batch_lookup_mode_miss")?
        {
            return Ok(result);
        }

        match self.resolve_pending_tx_lookups_force(*txid, mode)? {
            ForcedTxLookupOutcome::FoundModeMatch(tx) => {
                return Ok((Some(tx), "forced_batch_lookup"));
            }
            ForcedTxLookupOutcome::FoundModeMiss => {
                return Ok((None, "forced_batch_lookup_mode_miss"));
            }
            ForcedTxLookupOutcome::NotFound => {
                // Mark this txid as missing immediately after the forced batch query so concurrent
                // callers don't re-enqueue it while this lookup continues through miss resolution.
                self.cache.record_missing_txid(*txid)?;
                self.cache.enqueue_pending_tx_miss(*txid)?;
            }
            ForcedTxLookupOutcome::NotQueried => {
                debug_assert!(false, "forced lookup should always query requested txid");
            }
        }

        self.resolve_pending_tx_misses()?;
        if let Some(result) =
            self.lookup_cached_tx_with_mode(txid, mode, "batch_resolve", "batch_resolve_mode_miss")?
        {
            return Ok(result);
        }

        self.cache.record_missing_txid(*txid)?;
        self.cache.enqueue_pending_tx_miss(*txid)?;
        Ok((None, "db_miss"))
    }

    pub(super) fn lookup_tx(&self, txid: &Txid) -> Result<TxLookup, bdk::Error> {
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
    fn unresolved_txids_keeps_only_not_found_entries() {
        let first = Txid::all_zeros();
        let second = Txid::from_slice(&[1; 32]).expect("valid txid");
        let third = Txid::from_slice(&[2; 32]).expect("valid txid");

        let mut found = HashMap::new();
        found.insert(second, tx_details(second));

        let unresolved = SqlxWalletDb::unresolved_txids(vec![first, second, third], &found);
        assert_eq!(unresolved, vec![first, third]);
    }

    #[test]
    fn summary_tx_in_require_raw_mode_is_mode_miss_not_absent() {
        let tx = tx_details(Txid::all_zeros());

        assert!(!SqlxWalletDb::tx_matches_lookup_mode(
            &tx,
            TxLookupMode::RequireRaw
        ));
        assert!(SqlxWalletDb::tx_matches_lookup_mode(&tx, TxLookupMode::Any));
    }
}
