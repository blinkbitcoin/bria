use bdk::{bitcoin::Txid, BlockTime, LocalUtxo, TransactionDetails};
use futures::{TryStream, TryStreamExt};
use sqlx::{PgPool, Postgres, QueryBuilder, Transaction as SqlxTransaction};
use tracing::instrument;

use std::collections::HashMap;

use crate::{bdk::error::BdkError, primitives::*};

type SerializedTransactionRow = (String, serde_json::Value, i64, Option<i32>);

#[derive(Debug)]
pub struct UnsyncedTransaction {
    pub tx_id: bitcoin::Txid,
    pub confirmation_time: Option<bitcoin::BlockTime>,
    pub vsize: u64,
    pub total_utxo_in_sats: Satoshis,
    pub fee_sats: Satoshis,
    pub inputs: Vec<(LocalUtxo, u32)>,
    pub outputs: Vec<(LocalUtxo, u32)>,
}

pub struct ConfirmedSpendTransaction {
    #[allow(dead_code)]
    pub tx_id: bitcoin::Txid,
    pub confirmation_time: bitcoin::BlockTime,
    pub inputs: Vec<LocalUtxo>,
    pub outputs: Vec<LocalUtxo>,
}

pub struct Transactions {
    keychain_id: KeychainId,
    pool: PgPool,
}

impl Transactions {
    const LOAD_BATCH_SIZE: i64 = 10_000;

    fn parse_txid(tx_id: &str) -> Result<Txid, bdk::Error> {
        tx_id
            .parse::<Txid>()
            .map_err(|e| bdk::Error::Generic(format!("invalid tx_id in db: {e}")))
    }

    fn deserialize_details(
        details_json: serde_json::Value,
    ) -> Result<TransactionDetails, bdk::Error> {
        serde_json::from_value::<TransactionDetails>(details_json)
            .map_err(|e| bdk::Error::Generic(format!("could not deserialize tx details: {e}")))
    }

    fn to_u64(value: i64, field: &str) -> Result<u64, bdk::Error> {
        if value < 0 {
            return Err(bdk::Error::Generic(format!(
                "negative {field} value in bdk_transactions"
            )));
        }
        Ok(value as u64)
    }

    fn to_u32(value: i32, field: &str) -> Result<u32, bdk::Error> {
        if value < 0 {
            return Err(bdk::Error::Generic(format!(
                "negative {field} value in bdk_transactions"
            )));
        }
        Ok(value as u32)
    }

    async fn next_stream_row<T, S>(stream: &mut S) -> Result<Option<T>, bdk::Error>
    where
        S: TryStream<Ok = T, Error = sqlx::Error> + Unpin,
    {
        stream
            .try_next()
            .await
            .map_err(|e| bdk::Error::Generic(e.to_string()))
    }

    fn record_loaded_row(
        last_tx_id: &mut Option<String>,
        total_rows: &mut usize,
        batch_rows: &mut usize,
        tx_id: String,
    ) {
        *last_tx_id = Some(tx_id);
        *total_rows += 1;
        *batch_rows += 1;
    }

    fn serialize_batch(
        batch: &[TransactionDetails],
    ) -> Result<Vec<SerializedTransactionRow>, bdk::Error> {
        batch
            .iter()
            .map(|tx| {
                Ok::<_, bdk::Error>((
                    tx.txid.to_string(),
                    serde_json::to_value(tx).map_err(|e| {
                        bdk::Error::Generic(format!("failed to serialize tx details: {e}"))
                    })?,
                    tx.sent as i64,
                    tx.confirmation_time.as_ref().map(|t| t.height as i32),
                ))
            })
            .collect()
    }

    pub fn new(keychain_id: KeychainId, pool: PgPool) -> Self {
        Self { keychain_id, pool }
    }

    pub async fn persist_all_in_tx(
        &self,
        tx: &mut SqlxTransaction<'_, Postgres>,
        txs: Vec<TransactionDetails>,
    ) -> Result<(), bdk::Error> {
        const BATCH_SIZE: usize = 2000;
        let batches = txs.chunks(BATCH_SIZE);

        for batch in batches {
            let serialized_batch = Self::serialize_batch(batch)?;

            let mut query_builder: QueryBuilder<Postgres> = QueryBuilder::new(
                r#"
            INSERT INTO bdk_transactions
            (keychain_id, tx_id, details_json, sent, height)"#,
            );

            query_builder.push_values(
                serialized_batch,
                |mut builder, (tx_id, details_json, sent, height)| {
                    builder.push_bind(self.keychain_id as KeychainId);
                    builder.push_bind(tx_id);
                    builder.push_bind(details_json);
                    builder.push_bind(sent);
                    builder.push_bind(height);
                },
            );

            query_builder.push(
                "ON CONFLICT (keychain_id, tx_id) DO UPDATE \
                 SET details_json = EXCLUDED.details_json,\
                     sent = EXCLUDED.sent,\
                     height = EXCLUDED.height,\
                     modified_at = NOW(),\
                     deleted_at = NULL \
                 WHERE bdk_transactions.details_json IS DISTINCT FROM EXCLUDED.details_json \
                    OR bdk_transactions.sent IS DISTINCT FROM EXCLUDED.sent \
                    OR bdk_transactions.height IS DISTINCT FROM EXCLUDED.height \
                    OR bdk_transactions.deleted_at IS NOT NULL",
            );

            query_builder
                .build()
                .execute(tx.as_mut())
                .await
                .map_err(|e| bdk::Error::Generic(e.to_string()))?;
        }

        Ok(())
    }

    #[instrument(name = "bdk.transactions.delete", skip_all)]
    pub async fn delete(&self, tx_id: &Txid) -> Result<Option<TransactionDetails>, bdk::Error> {
        let tx = sqlx::query!(
            r#"UPDATE bdk_transactions
                 SET deleted_at = NOW()
                 WHERE keychain_id = $1 AND tx_id = $2
                 RETURNING details_json"#,
            self.keychain_id as KeychainId,
            tx_id.to_string(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| bdk::Error::Generic(e.to_string()))?;

        tx.map(|tx| {
            serde_json::from_value(tx.details_json)
                .map_err(|e| bdk::Error::Generic(format!("could not deserialize tx details: {e}")))
        })
        .transpose()
    }

    #[instrument(name = "bdk.transactions.find_by_id", skip_all)]
    pub async fn find_by_id(&self, tx_id: &Txid) -> Result<Option<TransactionDetails>, bdk::Error> {
        let tx = sqlx::query!(
            r#"
        SELECT details_json FROM bdk_transactions WHERE keychain_id = $1 AND tx_id = $2 AND deleted_at IS NULL"#,
            self.keychain_id as KeychainId,
            tx_id.to_string(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| bdk::Error::Generic(e.to_string()))?;
        tx.map(|tx| Self::deserialize_details(tx.details_json))
            .transpose()
    }

    #[instrument(name = "bdk.transactions.find_by_ids", skip_all, fields(n_requested = tx_ids.len(), n_found))]
    pub async fn find_by_ids(
        &self,
        tx_ids: &[Txid],
    ) -> Result<HashMap<Txid, TransactionDetails>, bdk::Error> {
        if tx_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let tx_ids_text: Vec<String> = tx_ids.iter().map(ToString::to_string).collect();
        let rows = sqlx::query!(
            r#"
        SELECT tx_id, details_json
          FROM bdk_transactions
         WHERE keychain_id = $1
           AND deleted_at IS NULL
           AND tx_id = ANY($2)"#,
            self.keychain_id as KeychainId,
            &tx_ids_text,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| bdk::Error::Generic(e.to_string()))?;

        tracing::Span::current().record("n_found", rows.len());

        rows.into_iter()
            .map(|row| {
                let txid = Self::parse_txid(&row.tx_id)?;
                let tx = Self::deserialize_details(row.details_json)?;
                Ok((txid, tx))
            })
            .collect()
    }

    #[instrument(name = "bdk.transactions.load_all", skip(self), fields(n_rows))]
    pub async fn load_all(&self) -> Result<HashMap<Txid, TransactionDetails>, bdk::Error> {
        let mut count = 0usize;
        let mut out = HashMap::new();
        let mut last_tx_id: Option<String> = None;

        loop {
            let mut stream = sqlx::query!(
                r#"
            SELECT tx_id, details_json
              FROM bdk_transactions
             WHERE keychain_id = $1
               AND deleted_at IS NULL
               AND ($2::TEXT IS NULL OR tx_id > $2)
             ORDER BY tx_id ASC
             LIMIT $3"#,
                self.keychain_id as KeychainId,
                last_tx_id,
                Self::LOAD_BATCH_SIZE,
            )
            .fetch(&self.pool);

            let mut batch_rows = 0usize;
            while let Some(row) = Self::next_stream_row(&mut stream).await? {
                let details = Self::deserialize_details(row.details_json)?;
                Self::record_loaded_row(&mut last_tx_id, &mut count, &mut batch_rows, row.tx_id);
                out.insert(details.txid, details);
            }

            if batch_rows == 0 {
                break;
            }
        }

        tracing::Span::current().record("n_rows", count);
        Ok(out)
    }

    #[instrument(
        name = "bdk.transactions.load_all_summaries",
        skip(self),
        fields(n_rows)
    )]
    pub async fn load_all_summaries(
        &self,
    ) -> Result<HashMap<Txid, TransactionDetails>, bdk::Error> {
        let mut count = 0usize;
        let mut out = HashMap::new();
        let mut last_tx_id: Option<String> = None;

        loop {
            let mut stream = sqlx::query!(
                r#"
            SELECT tx_id, sent, height,
                   (details_json->>'received')::BIGINT AS "received?",
                   (details_json->>'fee')::BIGINT AS "fee?",
                   (details_json->'confirmation_time'->>'timestamp')::BIGINT AS "confirmation_timestamp?"
              FROM bdk_transactions
             WHERE keychain_id = $1
               AND deleted_at IS NULL
               AND ($2::TEXT IS NULL OR tx_id > $2)
             ORDER BY tx_id ASC
             LIMIT $3"#,
                self.keychain_id as KeychainId,
                last_tx_id,
                Self::LOAD_BATCH_SIZE,
            )
            .fetch(&self.pool);

            let mut batch_rows = 0usize;
            while let Some(row) = Self::next_stream_row(&mut stream).await? {
                let txid = Self::parse_txid(&row.tx_id)?;

                let confirmation_time = match (row.height, row.confirmation_timestamp) {
                    (Some(height), Some(timestamp)) => Some(BlockTime {
                        height: Self::to_u32(height, "height")?,
                        timestamp: Self::to_u64(timestamp, "confirmation timestamp")?,
                    }),
                    _ => None,
                };

                let details = TransactionDetails {
                    txid,
                    transaction: None,
                    received: Self::to_u64(row.received.unwrap_or_default(), "received")?,
                    sent: Self::to_u64(row.sent, "sent")?,
                    fee: row.fee.map(|f| Self::to_u64(f, "fee")).transpose()?,
                    confirmation_time,
                };

                Self::record_loaded_row(&mut last_tx_id, &mut count, &mut batch_rows, row.tx_id);
                out.insert(txid, details);
            }

            if batch_rows == 0 {
                break;
            }
        }

        tracing::Span::current().record("n_rows", count);
        Ok(out)
    }

    #[instrument(name = "bdk.transactions.find_unsynced_tx", skip(self), fields(n_rows))]
    pub async fn find_unsynced_tx(
        &self,
        excluded_tx_ids: &[String],
    ) -> Result<Option<UnsyncedTransaction>, BdkError> {
        let rows = sqlx::query!(
        r#"WITH tx_to_sync AS (
           SELECT tx_id, details_json, height
           FROM bdk_transactions
           WHERE keychain_id = $1 AND synced_to_bria = false AND tx_id != ALL($2) AND deleted_at IS NULL
           ORDER BY height ASC NULLS LAST
           LIMIT 1
           ),
           previous_outputs AS (
               SELECT (jsonb_array_elements(details_json->'transaction'->'input')->>'previous_output') AS output
               FROM tx_to_sync
           )
           SELECT t.tx_id, details_json, utxo_json, path, vout,
                  CASE WHEN u.tx_id = t.tx_id THEN true ELSE false END AS "is_tx_output!"
           FROM bdk_utxos u
           JOIN tx_to_sync t ON u.tx_id = t.tx_id OR CONCAT(u.tx_id, ':', u.vout::text) = ANY(
               SELECT output FROM previous_outputs
           ) OR u.tx_id = t.tx_id
           JOIN bdk_script_pubkeys p
           ON p.keychain_id = $1 AND u.utxo_json->'txout'->>'script_pubkey' = p.script_hex
           WHERE u.keychain_id = $1 AND u.deleted_at IS NULL AND (u.synced_to_bria = false OR u.tx_id != t.tx_id)
        "#,
        self.keychain_id as KeychainId,
        &excluded_tx_ids
        )
           .fetch_all(&self.pool)
           .await?;

        tracing::Span::current().record("n_rows", rows.len());

        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let mut tx_id = None;
        let mut confirmation_time = None;
        let mut vsize = 0;

        let mut total_utxo_in_sats = Satoshis::ZERO;
        let mut fee_sats = Satoshis::ZERO;

        for row in rows {
            let utxo: LocalUtxo = serde_json::from_value(row.utxo_json)?;
            if row.is_tx_output {
                outputs.push((utxo, row.path as u32));
            } else {
                inputs.push((utxo, row.path as u32));
            }
            if tx_id.is_none() {
                tx_id = Some(row.tx_id.parse().map_err(|e| {
                    bdk::Error::Generic(format!("invalid tx id from bdk_transactions: {e}"))
                })?);
                let details: TransactionDetails = serde_json::from_value(row.details_json)?;
                total_utxo_in_sats = Satoshis::from(details.sent);
                fee_sats = Satoshis::from(details.fee.ok_or_else(|| {
                    bdk::Error::Generic("missing fee in unsynced transaction details".to_string())
                })?);
                vsize = details
                    .transaction
                    .ok_or_else(|| {
                        bdk::Error::Generic(
                            "missing raw transaction in unsynced transaction details".to_string(),
                        )
                    })?
                    .vsize() as u64;
                confirmation_time = details.confirmation_time;
            }
        }
        Ok(tx_id.map(|tx_id| UnsyncedTransaction {
            tx_id,
            total_utxo_in_sats,
            fee_sats,
            confirmation_time,
            vsize,
            inputs,
            outputs,
        }))
    }

    #[instrument(name = "bdk.transactions.find_confirmed_spend_tx", skip(self, tx))]
    pub async fn find_confirmed_spend_tx(
        &self,
        tx: &mut SqlxTransaction<'_, Postgres>,
        min_height: u32,
    ) -> Result<Option<ConfirmedSpendTransaction>, BdkError> {
        let rows = sqlx::query!(r#"
            WITH tx_to_sync AS (
              UPDATE bdk_transactions SET confirmation_synced_to_bria = true, modified_at = NOW()
              WHERE keychain_id = $1 AND tx_id IN (
                SELECT tx_id
                FROM bdk_transactions
                WHERE keychain_id = $1
                AND deleted_at IS NULL
                AND sent > 0
                AND height IS NOT NULL
                AND height <= $2
                AND synced_to_bria = true
                AND confirmation_synced_to_bria = false
                ORDER BY height ASC
                LIMIT 1)
                RETURNING tx_id, details_json
            ),
            previous_outputs AS (
                SELECT (jsonb_array_elements(details_json->'transaction'->'input')->>'previous_output') AS output
                FROM tx_to_sync
            )
            SELECT t.tx_id, details_json, utxo_json, vout,
                   CASE WHEN u.tx_id = t.tx_id THEN true ELSE false END AS "is_tx_output!"
            FROM bdk_utxos u
            JOIN tx_to_sync t ON u.tx_id = t.tx_id OR CONCAT(u.tx_id, ':', u.vout::text) = ANY(
                SELECT output FROM previous_outputs
            ) OR u.tx_id = t.tx_id
            WHERE u.keychain_id = $1 AND u.deleted_at IS NULL AND (u.confirmation_synced_to_bria = false OR u.tx_id != t.tx_id)
        "#,
            self.keychain_id as KeychainId,
            min_height as i32
        )
        .fetch_all(&mut **tx)
        .await?;

        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let mut tx_id = None;
        let mut confirmation_time = None;

        for row in rows {
            let utxo: LocalUtxo = serde_json::from_value(row.utxo_json)?;
            if row.is_tx_output {
                outputs.push(utxo);
            } else {
                inputs.push(utxo);
            }
            if tx_id.is_none() {
                tx_id = Some(row.tx_id.parse().map_err(|e| {
                    bdk::Error::Generic(format!("invalid tx id from bdk_transactions: {e}"))
                })?);
                let details: TransactionDetails = serde_json::from_value(row.details_json)?;
                confirmation_time = details.confirmation_time;
            }
        }

        if let Some(tx_id) = tx_id {
            let confirmation_time = confirmation_time.ok_or_else(|| {
                bdk::Error::Generic(
                    "missing confirmation_time in confirmed spend transaction details".to_string(),
                )
            })?;
            Ok(Some(ConfirmedSpendTransaction {
                tx_id,
                confirmation_time,
                inputs,
                outputs,
            }))
        } else {
            Ok(None)
        }
    }

    #[instrument(name = "bdk.transactions.mark_as_synced", skip(self))]
    pub async fn mark_as_synced(&self, tx_id: bitcoin::Txid) -> Result<(), BdkError> {
        sqlx::query!(
            r#"UPDATE bdk_transactions SET synced_to_bria = true, modified_at = NOW()
            WHERE keychain_id = $1 AND tx_id = $2"#,
            self.keychain_id as KeychainId,
            tx_id.to_string(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[instrument(name = "bdk.transactions.mark_confirmed", skip(self))]
    pub async fn mark_confirmed(
        &self,
        tx: &mut SqlxTransaction<'_, Postgres>,
        tx_id: bitcoin::Txid,
    ) -> Result<(), BdkError> {
        sqlx::query!(
            r#"UPDATE bdk_transactions SET confirmation_synced_to_bria = true, modified_at = NOW()
            WHERE keychain_id = $1 AND tx_id = $2"#,
            self.keychain_id as KeychainId,
            tx_id.to_string(),
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    #[instrument(
        name = "bdk.transactions.delete_transaction_if_no_more_utxos_exist",
        skip(self, tx)
    )]
    pub async fn delete_transaction_if_no_more_utxos_exist(
        &self,
        tx: &mut SqlxTransaction<'_, Postgres>,
        outpoint: bitcoin::OutPoint,
    ) -> Result<(), BdkError> {
        sqlx::query!(
            r#"
            DELETE FROM bdk_transactions
            WHERE keychain_id = $1 AND  tx_id = $2 AND NOT EXISTS (
                SELECT 1 FROM bdk_utxos WHERE keychain_id = $1 AND tx_id = $2
            )
            "#,
            self.keychain_id as KeychainId,
            outpoint.txid.to_string(),
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}
