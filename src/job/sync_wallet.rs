use bdk::{
    blockchain::{ElectrumBlockchain, GetHeight},
    wallet::AddressInfo,
    LocalUtxo,
};
use electrum_client::{Client, ConfigBuilder};
use serde::{Deserialize, Serialize};
use tracing::{info, instrument, warn};

use super::error::JobError;
use crate::{
    address::*,
    app::BlockchainConfig,
    batch::*,
    bdk::{
        error::BdkError,
        pg::{
            ConfirmedIncomeUtxo, ConfirmedSpendTransaction, Transactions, UnsyncedTransaction,
            Utxos as BdkUtxos,
        },
    },
    fees::{self, FeesClient},
    ledger::*,
    primitives::*,
    utxo::{error::UtxoError, Utxos, WalletUtxo},
    wallet::*,
};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncWalletData {
    pub(super) account_id: AccountId,
    pub(super) wallet_id: WalletId,
}

impl SyncWalletData {
    pub fn new(account_id: AccountId, wallet_id: WalletId) -> Self {
        SyncWalletData {
            account_id,
            wallet_id,
        }
    }
}

struct InstrumentationTrackers {
    n_pending_utxos: usize,
    n_confirmed_utxos: usize,
    n_found_txs: usize,
}

impl InstrumentationTrackers {
    fn new() -> Self {
        InstrumentationTrackers {
            n_pending_utxos: 0,
            n_confirmed_utxos: 0,
            n_found_txs: 0,
        }
    }
}

struct Deps {
    blockchain_cfg: BlockchainConfig,
    bria_addresses: Addresses,
    bria_utxos: Utxos,
    ledger: Ledger,
}

struct KeychainSyncContext<'a> {
    pool: &'a sqlx::PgPool,
    data: &'a SyncWalletData,
    wallet: &'a Wallet,
    keychain_wallet: &'a KeychainWallet,
    deps: &'a Deps,
    bdk_txs: &'a Transactions,
    bdk_utxos: &'a BdkUtxos,
    batches: &'a Batches,
    keychain_id: KeychainId,
    current_height: u32,
    fees_to_encumber: Satoshis,
}

enum SpendInputState {
    CompleteInputs {
        income_bria_utxos: Vec<WalletUtxo>,
    },
    MissingInputs {
        expected: usize,
        found: usize,
        missing_outpoints: Vec<bitcoin::OutPoint>,
    },
}

enum SpendOutcome {
    Applied,
    Deferred,
}

const MAX_TXS_PER_SYNC: usize = 100;

#[instrument(
    name = "job.sync_wallet",
    skip(pool, wallets, batches, bria_utxos, bria_addresses, ledger),
    fields(
        n_pending_utxos,
        n_confirmed_utxos,
        n_found_txs,
        has_more,
        current_height
    ),
    err
)]
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    pool: sqlx::PgPool,
    wallets: Wallets,
    blockchain_cfg: BlockchainConfig,
    bria_utxos: Utxos,
    bria_addresses: Addresses,
    ledger: Ledger,
    batches: Batches,
    data: SyncWalletData,
    fees_client: FeesClient,
) -> Result<(bool, SyncWalletData), JobError> {
    info!("Starting sync_wallet job: {:?}", data);
    let span = tracing::Span::current();

    let wallet = wallets.find_by_id(data.wallet_id).await?;
    let mut trackers = InstrumentationTrackers::new();
    let deps = Deps {
        blockchain_cfg,
        bria_addresses,
        bria_utxos,
        ledger,
    };

    for keychain_wallet in wallet.keychain_wallets(pool.clone()) {
        info!("Syncing keychain '{}'", keychain_wallet.keychain_id);

        let fees_to_encumber =
            fees::fees_to_encumber(&fees_client, keychain_wallet.max_satisfaction_weight()).await?;
        let keychain_id = keychain_wallet.keychain_id;

        let (blockchain, current_height) = init_electrum(&deps.blockchain_cfg.electrum_url).await?;
        span.record("current_height", current_height);

        keychain_wallet.sync(blockchain).await?;

        let bdk_txs = Transactions::new(keychain_id, pool.clone());
        let bdk_utxos = BdkUtxos::new(keychain_id, pool.clone());

        let ctx = KeychainSyncContext {
            pool: &pool,
            data: &data,
            wallet: &wallet,
            keychain_wallet: &keychain_wallet,
            deps: &deps,
            bdk_txs: &bdk_txs,
            bdk_utxos: &bdk_utxos,
            batches: &batches,
            keychain_id,
            current_height,
            fees_to_encumber,
        };

        process_unsynced_txs(&ctx, &mut trackers).await?;
        settle_confirmed_income_utxos(&ctx, &mut trackers).await?;
        settle_confirmed_spend_txs(&ctx).await?;
        cleanup_soft_deleted_utxos(&ctx).await?;
    }

    let has_more = trackers.n_found_txs >= MAX_TXS_PER_SYNC;
    span.record("n_pending_utxos", trackers.n_pending_utxos);
    span.record("n_confirmed_utxos", trackers.n_confirmed_utxos);
    span.record("n_found_txs", trackers.n_found_txs);
    span.record("has_more", has_more);

    Ok((has_more, data))
}

// Processes unsynced BDK transactions: detects new income UTXOs, records spend
// transactions, and immediately settles any that are confirmed deep enough.
async fn process_unsynced_txs(
    ctx: &KeychainSyncContext<'_>,
    trackers: &mut InstrumentationTrackers,
) -> Result<(), JobError> {
    let latest_change_settle_height = ctx
        .wallet
        .config
        .latest_change_settle_height(ctx.current_height);
    let mut txs_to_skip = Vec::new();

    while let Ok(Some(mut unsynced_tx)) = ctx.bdk_txs.find_unsynced_tx(&txs_to_skip).await {
        tracing::info!(?unsynced_tx);
        trackers.n_found_txs += 1;

        let input_outpoints: Vec<bitcoin::OutPoint> =
            unsynced_tx.inputs.iter().map(|i| i.0.outpoint).collect();
        let is_spend_tx = !input_outpoints.is_empty();

        // Batch broadcast is recorded before input validation intentionally.
        // During incident recovery, inputs from a prior wallet may not yet be synced
        // when this tx is first seen. Recording the broadcast ledger entry early ensures
        // it is not lost; spend accounting is deferred until inputs converge on a later
        // sync cycle. This creates a temporary window where a broadcast entry exists
        // without a matching spend_detected entry — this is expected and observable via
        // the "batch_broadcast_recorded_while_spend_deferred" log event.
        let batch_broadcast_info = if is_spend_tx {
            maybe_record_batch_broadcast(ctx, &unsynced_tx).await?
        } else {
            None
        };

        let income_bria_utxos = if is_spend_tx {
            let spend_input_state = spend_input_state(ctx, &input_outpoints).await?;
            match spend_input_state {
                SpendInputState::CompleteInputs { income_bria_utxos } => income_bria_utxos,
                SpendInputState::MissingInputs {
                    expected,
                    found,
                    missing_outpoints,
                } => {
                    if let Some((batch_info, batch_broadcast_ledger_tx_id)) =
                        batch_broadcast_info.as_ref()
                    {
                        info!(
                            message = "batch_broadcast_recorded_while_spend_deferred",
                            wallet_id = %ctx.wallet.id,
                            keychain_id = %ctx.keychain_id,
                            tx_id = %unsynced_tx.tx_id,
                            batch_id = %batch_info.id,
                            batch_broadcast_ledger_tx_id = %batch_broadcast_ledger_tx_id,
                        );
                    }
                    warn!(
                        message = "spend_inputs_missing",
                        wallet_id = %ctx.wallet.id,
                        keychain_id = %ctx.keychain_id,
                        tx_id = %unsynced_tx.tx_id,
                        expected,
                        found,
                        ?missing_outpoints,
                    );
                    txs_to_skip.push(unsynced_tx.tx_id.to_string());
                    continue;
                }
            }
        } else {
            Vec::new()
        };
        txs_to_skip.clear();

        let mut change_outputs = Vec::new();
        let mut income_outputs = Vec::new();
        for output in unsynced_tx.outputs.drain(..) {
            if is_spend_tx && output.0.keychain == bitcoin::KeychainKind::Internal {
                change_outputs.push(output);
            } else {
                income_outputs.push(output);
            }
        }

        for (local_utxo, path) in income_outputs {
            let address_info = ctx
                .keychain_wallet
                .find_address_from_path(path, local_utxo.keychain)
                .await?;

            let found_addr = NewAddress::builder()
                .account_id(ctx.data.account_id)
                .wallet_id(ctx.data.wallet_id)
                .keychain_id(ctx.keychain_id)
                .address(address_info.address.clone().into())
                .kind(address_info.keychain)
                .address_idx(address_info.index)
                .metadata(Some(address_metadata(&unsynced_tx.tx_id)))
                .build()
                .expect("Could not build new address in sync wallet");

            if let Some((pending_id, mut tx)) = ctx
                .deps
                .bria_utxos
                .new_utxo_detected(
                    ctx.data.account_id,
                    ctx.wallet.id,
                    ctx.keychain_id,
                    &address_info,
                    &local_utxo,
                    unsynced_tx.fee_sats,
                    unsynced_tx.vsize,
                    is_spend_tx,
                    ctx.current_height,
                )
                .await?
            {
                trackers.n_pending_utxos += 1;
                ctx.deps
                    .bria_addresses
                    .persist_if_not_present(&mut tx, found_addr)
                    .await?;
                ctx.bdk_utxos.mark_as_synced(&mut tx, &local_utxo).await?;
                ctx.deps
                    .ledger
                    .utxo_detected(
                        tx,
                        pending_id,
                        UtxoDetectedParams {
                            journal_id: ctx.wallet.journal_id,
                            onchain_incoming_account_id: ctx
                                .wallet
                                .ledger_account_ids
                                .onchain_incoming_id,
                            effective_incoming_account_id: ctx
                                .wallet
                                .ledger_account_ids
                                .effective_incoming_id,
                            onchain_fee_account_id: ctx.wallet.ledger_account_ids.fee_id,
                            meta: UtxoDetectedMeta {
                                account_id: ctx.data.account_id,
                                wallet_id: ctx.data.wallet_id,
                                keychain_id: ctx.keychain_id,
                                outpoint: local_utxo.outpoint,
                                satoshis: local_utxo.txout.value.into(),
                                address: address_info.address.into(),
                                encumbered_spending_fees: std::iter::once((
                                    local_utxo.outpoint,
                                    ctx.fees_to_encumber,
                                ))
                                .collect(),
                                confirmation_time: unsynced_tx.confirmation_time.clone(),
                            },
                        },
                    )
                    .await?;

                // If the UTXO is already confirmed deep enough, settle it immediately.
                let settle_height = ctx
                    .wallet
                    .config
                    .latest_settle_height(ctx.current_height, is_spend_tx);
                if let Some(conf_time) = unsynced_tx
                    .confirmation_time
                    .as_ref()
                    .filter(|t| t.height <= settle_height)
                {
                    let mut settle_tx = ctx.pool.begin().await?;
                    ctx.bdk_utxos
                        .mark_confirmed(&mut settle_tx, &local_utxo)
                        .await?;
                    let utxo = ctx
                        .deps
                        .bria_utxos
                        .settle_utxo(
                            &mut settle_tx,
                            ctx.keychain_id,
                            local_utxo.outpoint,
                            local_utxo.is_spent,
                            conf_time.height,
                        )
                        .await?;
                    trackers.n_confirmed_utxos += 1;
                    ctx.deps
                        .ledger
                        .utxo_settled(
                            settle_tx,
                            utxo.utxo_settled_ledger_tx_id,
                            UtxoSettledParams {
                                journal_id: ctx.wallet.journal_id,
                                ledger_account_ids: ctx.wallet.ledger_account_ids,
                                pending_id: utxo.utxo_detected_ledger_tx_id,
                                meta: UtxoSettledMeta {
                                    account_id: ctx.data.account_id,
                                    wallet_id: ctx.data.wallet_id,
                                    keychain_id: ctx.keychain_id,
                                    confirmation_time: conf_time.clone(),
                                    satoshis: utxo.value,
                                    outpoint: local_utxo.outpoint,
                                    address: utxo.address,
                                    already_spent_tx_id: utxo.spend_detected_ledger_tx_id,
                                },
                            },
                        )
                        .await?;
                }
            }
        }

        if is_spend_tx {
            let outcome = process_spend_tx(
                ctx,
                &unsynced_tx,
                &income_bria_utxos,
                &change_outputs,
                batch_broadcast_info,
            )
            .await?;
            if let SpendOutcome::Deferred = outcome {
                txs_to_skip.push(unsynced_tx.tx_id.to_string());
                continue;
            }
        }

        ctx.bdk_txs.mark_as_synced(unsynced_tx.tx_id).await?;

        // If the spend tx is confirmed and deep enough, settle it immediately.
        if let Some(conf_time) = unsynced_tx.confirmation_time {
            if is_spend_tx && conf_time.height <= latest_change_settle_height {
                let mut settle_tx = ctx.pool.begin().await?;
                if let Some((pending_out_id, confirmed_out_id, change_spent)) = ctx
                    .deps
                    .bria_utxos
                    .spend_settled(
                        &mut settle_tx,
                        ctx.keychain_id,
                        input_outpoints.iter(),
                        change_outputs.first().map(|(u, _)| u.clone()),
                        conf_time.height,
                    )
                    .await?
                {
                    ctx.bdk_txs
                        .mark_confirmed(&mut settle_tx, unsynced_tx.tx_id)
                        .await?;
                    ctx.deps
                        .ledger
                        .spend_settled(
                            settle_tx,
                            confirmed_out_id,
                            ctx.wallet.journal_id,
                            ctx.wallet.ledger_account_ids,
                            pending_out_id,
                            conf_time,
                            change_spent,
                        )
                        .await?;
                }
            }
        }

        if trackers.n_found_txs >= MAX_TXS_PER_SYNC {
            break;
        }
    }

    Ok(())
}

// Records a spend transaction: resolves its batch (if any), persists change
// addresses, and records spend_detected in the ledger.
async fn process_spend_tx(
    ctx: &KeychainSyncContext<'_>,
    unsynced_tx: &UnsyncedTransaction,
    income_bria_utxos: &[WalletUtxo],
    change_outputs: &[(LocalUtxo, u32)],
    batch_broadcast_info: Option<(BatchInfo, LedgerTransactionId)>,
) -> Result<SpendOutcome, JobError> {
    let (mut tx, batch_info, tx_id) = if let Some((batch_info, tx_id)) = batch_broadcast_info {
        (ctx.pool.begin().await?, Some(batch_info), tx_id)
    } else {
        (ctx.pool.begin().await?, None, LedgerTransactionId::new())
    };

    let mut change_utxos: Vec<(&LocalUtxo, AddressInfo)> = Vec::new();
    let mut change_addrs = Vec::new();
    for (utxo, path) in change_outputs {
        let address_info = ctx
            .keychain_wallet
            .find_address_from_path(*path, utxo.keychain)
            .await?;
        let found_addr = NewAddress::builder()
            .account_id(ctx.data.account_id)
            .wallet_id(ctx.data.wallet_id)
            .keychain_id(ctx.keychain_id)
            .address(address_info.address.clone().into())
            .kind(address_info.keychain)
            .address_idx(address_info.index)
            .metadata(Some(address_metadata(&unsynced_tx.tx_id)))
            .build()
            .expect("Could not build new address in sync wallet");
        change_addrs.push(found_addr);
        change_utxos.push((utxo, address_info));
    }

    let spend_detected = ctx
        .deps
        .bria_utxos
        .spend_detected(
            &mut tx,
            ctx.data.account_id,
            ctx.wallet.id,
            ctx.keychain_id,
            tx_id,
            income_bria_utxos
                .iter()
                .map(|WalletUtxo { outpoint, .. }| outpoint),
            &change_utxos,
            batch_info
                .as_ref()
                .map(|info| (info.id, info.payout_queue_id)),
            unsynced_tx.fee_sats,
            unsynced_tx.vsize,
            ctx.current_height,
        )
        .await?;

    if let Some((settled_sats, allocations)) = spend_detected {
        for addr in change_addrs {
            ctx.deps
                .bria_addresses
                .persist_if_not_present(&mut tx, addr)
                .await?;
        }

        if batch_info.is_none() {
            let reserved_fees = ctx
                .deps
                .ledger
                .sum_reserved_fees_in_txs(income_bria_utxos.iter().fold(
                    HashMap::new(),
                    |mut m, u| {
                        m.entry(u.utxo_detected_ledger_tx_id)
                            .or_default()
                            .push(u.outpoint);
                        m
                    },
                ))
                .await?;
            ctx.deps
                .ledger
                .spend_detected(
                    tx,
                    tx_id,
                    SpendDetectedParams {
                        journal_id: ctx.wallet.journal_id,
                        ledger_account_ids: ctx.wallet.ledger_account_ids,
                        reserved_fees,
                        meta: SpendDetectedMeta {
                            encumbered_spending_fees: change_utxos
                                .iter()
                                .map(|(u, _)| (u.outpoint, ctx.fees_to_encumber))
                                .collect(),
                            withdraw_from_effective_when_settled: allocations,
                            tx_summary: WalletTransactionSummary {
                                account_id: ctx.data.account_id,
                                wallet_id: ctx.wallet.id,
                                current_keychain_id: ctx.keychain_id,
                                bitcoin_tx_id: unsynced_tx.tx_id,
                                total_utxo_in_sats: unsynced_tx.total_utxo_in_sats,
                                total_utxo_settled_in_sats: settled_sats,
                                fee_sats: unsynced_tx.fee_sats,
                                cpfp_details: None,
                                cpfp_fee_sats: None,
                                change_utxos: change_utxos
                                    .iter()
                                    .map(|(u, a)| ChangeOutput {
                                        outpoint: u.outpoint,
                                        address: a.address.clone().into(),
                                        satoshis: Satoshis::from(u.txout.value),
                                    })
                                    .collect(),
                            },
                            confirmation_time: unsynced_tx.confirmation_time.clone(),
                        },
                    },
                )
                .await?;
        } else {
            tx.commit().await?;
        }

        return Ok(SpendOutcome::Applied);
    }

    warn!(
        message = "spend_detected_deferred",
        wallet_id = %ctx.wallet.id,
        keychain_id = %ctx.keychain_id,
        tx_id = %unsynced_tx.tx_id,
    );
    Ok(SpendOutcome::Deferred)
}

async fn maybe_record_batch_broadcast(
    ctx: &KeychainSyncContext<'_>,
    unsynced_tx: &UnsyncedTransaction,
) -> Result<Option<(BatchInfo, LedgerTransactionId)>, JobError> {
    let Some((tx, batch_info, tx_id, was_newly_set)) = ctx
        .batches
        .set_batch_broadcast_ledger_tx_id(unsynced_tx.tx_id, ctx.wallet.id)
        .await?
    else {
        return Ok(None);
    };

    if was_newly_set {
        ctx.deps
            .ledger
            .batch_broadcast(
                tx,
                batch_info.created_ledger_tx_id,
                tx_id,
                ctx.fees_to_encumber,
                ctx.wallet.ledger_account_ids,
            )
            .await?;
    } else {
        tx.commit().await?;
    }

    Ok(Some((batch_info, tx_id)))
}

async fn spend_input_state(
    ctx: &KeychainSyncContext<'_>,
    input_outpoints: &[bitcoin::OutPoint],
) -> Result<SpendInputState, JobError> {
    let utxos_by_keychain = HashMap::from([(ctx.keychain_id, input_outpoints.to_vec())]);
    let found = ctx
        .deps
        .bria_utxos
        .list_utxos_by_outpoint(&utxos_by_keychain)
        .await?;

    if found.len() == input_outpoints.len() {
        return Ok(SpendInputState::CompleteInputs {
            income_bria_utxos: found,
        });
    }

    let found_outpoints = found
        .iter()
        .map(|WalletUtxo { outpoint, .. }| *outpoint)
        .collect::<std::collections::HashSet<_>>();
    let missing_outpoints = input_outpoints
        .iter()
        .copied()
        .filter(|outpoint| !found_outpoints.contains(outpoint))
        .take(10)
        .collect::<Vec<_>>();

    Ok(SpendInputState::MissingInputs {
        expected: input_outpoints.len(),
        found: found.len(),
        missing_outpoints,
    })
}

// Settles income UTXOs that BDK has confirmed but bria hasn't settled yet.
async fn settle_confirmed_income_utxos(
    ctx: &KeychainSyncContext<'_>,
    trackers: &mut InstrumentationTrackers,
) -> Result<(), JobError> {
    let min_height = ctx
        .wallet
        .config
        .latest_income_settle_height(ctx.current_height);
    loop {
        let mut tx = ctx.pool.begin().await?;
        let confirmed = ctx
            .bdk_utxos
            .find_confirmed_income_utxo(&mut tx, min_height)
            .await;
        let Ok(Some(ConfirmedIncomeUtxo {
            outpoint,
            spent,
            confirmation_time,
        })) = confirmed
        else {
            break;
        };

        let utxo = ctx
            .deps
            .bria_utxos
            .settle_utxo(
                &mut tx,
                ctx.keychain_id,
                outpoint,
                spent,
                confirmation_time.height,
            )
            .await?;
        trackers.n_confirmed_utxos += 1;

        ctx.deps
            .ledger
            .utxo_settled(
                tx,
                utxo.utxo_settled_ledger_tx_id,
                UtxoSettledParams {
                    journal_id: ctx.wallet.journal_id,
                    ledger_account_ids: ctx.wallet.ledger_account_ids,
                    pending_id: utxo.utxo_detected_ledger_tx_id,
                    meta: UtxoSettledMeta {
                        account_id: ctx.data.account_id,
                        wallet_id: ctx.data.wallet_id,
                        keychain_id: ctx.keychain_id,
                        confirmation_time,
                        satoshis: utxo.value,
                        outpoint,
                        address: utxo.address,
                        already_spent_tx_id: utxo.spend_detected_ledger_tx_id,
                    },
                },
            )
            .await?;
    }
    Ok(())
}

// Settles spend transactions (and their change UTXOs) that BDK has confirmed
// but bria hasn't settled yet.
async fn settle_confirmed_spend_txs(ctx: &KeychainSyncContext<'_>) -> Result<(), JobError> {
    let min_height = ctx
        .wallet
        .config
        .latest_change_settle_height(ctx.current_height);
    loop {
        let mut tx = ctx.pool.begin().await?;
        let confirmed = ctx
            .bdk_txs
            .find_confirmed_spend_tx(&mut tx, min_height)
            .await;
        let Ok(Some(ConfirmedSpendTransaction {
            confirmation_time,
            inputs,
            outputs,
            ..
        })) = confirmed
        else {
            break;
        };

        let change_utxo = outputs
            .into_iter()
            .find(|u| u.keychain == bitcoin::KeychainKind::Internal);

        if let Some((pending_out_id, confirmed_out_id, change_spent)) = ctx
            .deps
            .bria_utxos
            .spend_settled(
                &mut tx,
                ctx.keychain_id,
                inputs.iter().map(|u| &u.outpoint),
                change_utxo,
                confirmation_time.height,
            )
            .await?
        {
            ctx.deps
                .ledger
                .spend_settled(
                    tx,
                    confirmed_out_id,
                    ctx.wallet.journal_id,
                    ctx.wallet.ledger_account_ids,
                    pending_out_id,
                    confirmation_time,
                    change_spent,
                )
                .await?;
        }
    }
    Ok(())
}

// Removes soft-deleted UTXOs (e.g. from reorgs) and reverses their ledger entries.
async fn cleanup_soft_deleted_utxos(ctx: &KeychainSyncContext<'_>) -> Result<(), JobError> {
    loop {
        let mut tx = ctx.pool.begin().await?;
        let Some((outpoint, keychain_id)) = ctx
            .bdk_utxos
            .find_and_remove_soft_deleted_utxo(&mut tx)
            .await?
        else {
            break;
        };

        ctx.bdk_txs
            .delete_transaction_if_no_more_utxos_exist(&mut tx, outpoint)
            .await?;

        let detected_txn_id = match ctx
            .deps
            .bria_utxos
            .delete_utxo(&mut tx, outpoint, keychain_id)
            .await
        {
            Ok(txn_id) => txn_id,
            Err(UtxoError::UtxoDoesNotExistError) => {
                tx.commit().await?;
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        match ctx
            .deps
            .ledger
            .utxo_dropped(tx, LedgerTransactionId::new(), detected_txn_id)
            .await
        {
            Ok(_) => (),
            Err(LedgerError::MismatchedTxMetadata(_)) => ctx.bdk_utxos.undelete(outpoint).await?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn init_electrum(electrum_url: &str) -> Result<(ElectrumBlockchain, u32), BdkError> {
    let blockchain = ElectrumBlockchain::from(Client::from_config(
        electrum_url,
        ConfigBuilder::new().retry(10).timeout(Some(60)).build(),
    )?);
    let current_height = blockchain.get_height()?;
    Ok((blockchain, current_height))
}

fn address_metadata(tx_id: &bitcoin::Txid) -> serde_json::Value {
    serde_json::json! {
        {
            "synced_in_tx": tx_id.to_string(),
        }
    }
}
