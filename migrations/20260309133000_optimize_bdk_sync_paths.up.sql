CREATE INDEX IF NOT EXISTS bdk_transactions_unsynced_by_height_idx
ON bdk_transactions (keychain_id, height ASC NULLS LAST, tx_id)
WHERE synced_to_bria = false AND deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS bdk_transactions_confirmed_spend_sync_idx
ON bdk_transactions (keychain_id, height, tx_id)
WHERE deleted_at IS NULL
  AND sent > 0
  AND height IS NOT NULL
  AND synced_to_bria = true
  AND confirmation_synced_to_bria = false;

CREATE INDEX IF NOT EXISTS bdk_utxos_pending_confirmation_sync_idx
ON bdk_utxos (keychain_id, tx_id, vout)
WHERE deleted_at IS NULL
  AND synced_to_bria = true
  AND confirmation_synced_to_bria = false;

CREATE INDEX IF NOT EXISTS bdk_utxos_soft_deleted_idx
ON bdk_utxos (keychain_id, deleted_at)
WHERE deleted_at IS NOT NULL;
