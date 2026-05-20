use electrum_client::{Client, ConfigBuilder, ElectrumApi};
use serde::{Deserialize, Serialize};
use tracing::instrument;

use std::collections::HashMap;

use super::error::JobError;
use crate::{app::BlockchainConfig, batch::*, bdk::error::BdkError, primitives::*};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchBroadcastingData {
    pub(super) account_id: AccountId,
    pub(super) batch_id: BatchId,
    #[serde(flatten)]
    pub(super) tracing_data: HashMap<String, String>,
}

#[instrument(
    name = "job.batch_broadcasting",
    skip(batches),
    fields(txid, broadcast = false),
    err
)]
pub async fn execute(
    data: BatchBroadcastingData,
    blockchain_cfg: BlockchainConfig,
    batches: Batches,
) -> Result<BatchBroadcastingData, JobError> {
    let client = init_electrum(&blockchain_cfg.electrum_url).await?;
    let batch = batches.find_by_id(data.account_id, data.batch_id).await?;
    let span = tracing::Span::current();
    span.record("txid", tracing::field::display(batch.bitcoin_tx_id));
    if let Some(tx) = batch.get_tx_to_broadcast() {
        broadcast_or_verify(&client, &tx, &data)?;
        span.record("broadcast", true);
    }
    Ok(data)
}

fn broadcast_or_verify(
    client: &Client,
    tx: &bitcoin::Transaction,
    data: &BatchBroadcastingData,
) -> Result<(), JobError> {
    let txid = tx.txid();
    if let Err(err) = client.transaction_broadcast(tx) {
        if is_tx_known(client, txid)? {
            tracing::info!(
                batch_id = %data.batch_id,
                txid = %txid,
                error = %err,
                "Broadcast returned error but transaction is already known by electrum; treating as idempotent success"
            );
        } else {
            return Err(BdkError::ElectrumClient(err).into());
        }
    }
    Ok(())
}

fn is_tx_known(client: &Client, txid: bitcoin::Txid) -> Result<bool, BdkError> {
    match client.transaction_get(&txid) {
        Ok(_) => Ok(true),
        Err(electrum_client::Error::Protocol(value)) if is_protocol_tx_not_found(&value) => {
            Ok(false)
        }
        Err(err) => Err(err.into()),
    }
}

fn is_protocol_tx_not_found(value: &serde_json::Value) -> bool {
    let code = value.get("code").and_then(serde_json::Value::as_i64);
    let msg = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    code == Some(-5)
        || msg.contains("'code': -5")
        || msg.contains("no such mempool or blockchain transaction")
        || msg.contains("no such transaction")
        || msg.contains("transaction not found")
}

async fn init_electrum(electrum_url: &str) -> Result<Client, BdkError> {
    let client = Client::from_config(
        electrum_url,
        ConfigBuilder::new().retry(10).timeout(Some(60)).build(),
    )?;
    Ok(client)
}
