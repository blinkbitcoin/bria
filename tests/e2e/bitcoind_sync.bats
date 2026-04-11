#!/usr/bin/env bats

load "helpers"

setup_file() {
  restart_bitcoin_stack
  reset_pg
  bitcoind_init
  start_daemon
  bria_init
}

teardown_file() {
  stop_daemon
}

@test "bitcoind_signer_sync: Generates the same address" {
  bitcoind_signer_address=$(bitcoin_signer_cli getnewaddress)
  bria_address=$(bria_cmd new-address -w default | jq -r '.address')

  [ "$bitcoind_signer_address" = "$bria_address" ] || exit 1

  n_addresses=$(bria_cmd list-addresses -w default | jq -r '.addresses | length')
  [ "$n_addresses" = "1" ] || exit 1
}

@test "bitcoind_signer_sync: Detects incoming transactions" {
  bitcoind_signer_address=$(bitcoin_signer_cli getnewaddress)
  if [ -z "$bitcoind_signer_address" ]; then
    echo "Failed to get a new address"
    exit 1
  fi

  bitcoin_cli -regtest sendtoaddress ${bitcoind_signer_address} 1

  retry 60 1 wallet_pending_income_is 100000000
  wallet_pending_income_is 100000000 || exit 1

  n_addresses=$(bria_cmd list-addresses -w default | jq -r '.addresses | length')
  [ "$n_addresses" = "2" ] || exit 1
  utxos=$(bria_cmd list-utxos -w default)
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  utxo_block_height=$(jq -r '.keychains[0].utxos[0].blockHeight' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${utxo_block_height}" == "null" ]]

  bitcoin_cli -generate 2

  retry 60 1 wallet_current_settled_is 100000000
  wallet_current_settled_is 100000000 || exit 1

  utxos=$(bria_cmd list-utxos -w default)
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  utxo_block_height=$(jq -r '.keychains[0].utxos[0].blockHeight' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${utxo_block_height}" == "201" ]] || exit 1
}

@test "bitcoind_signer_sync: Detects outgoing transactions" {
  bitcoind_address=$(bitcoin_cli -regtest getnewaddress)
  bitcoin_signer_cli -regtest sendtoaddress "${bitcoind_address}" 0.5
  retry 60 1 wallet_pending_outgoing_is 50000000
  wallet_pending_outgoing_is 50000000 || exit 1
  retry 60 1 wallet_current_settled_is 0
  wallet_current_settled_is 0 || exit 1

  utxos=$(bria_cmd list-utxos -w default)
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  change=$(jq -r '.keychains[0].utxos[0].changeOutput' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${change}" == "true" ]] || exit 1

  bitcoin_cli -generate 1

  retry 60 1 wallet_current_settled_is_not 0
  wallet_current_settled_is_not 0 || exit 1
  retry 60 1 wallet_pending_outgoing_is 0
  wallet_pending_outgoing_is 0 || exit 1

  utxos=$(bria_cmd list-utxos -w default)
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  utxo_block_height=$(jq -r '.keychains[0].utxos[0].blockHeight' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${utxo_block_height}" == "203" ]] || exit 1
}

@test "bitcoind_signer_sync: Can handle spend from mix of unconfirmed UTXOs" {
  bitcoind_signer_address=$(bitcoin_signer_cli getnewaddress)
  if [ -z "$bitcoind_signer_address" ]; then
    echo "Failed to get a new address"
    exit 1
  fi

  bitcoin_cli -regtest sendtoaddress ${bitcoind_signer_address} 1
  bitcoin_cli -regtest sendtoaddress ${bitcoind_signer_address} 1

  bitcoind_address=$(bitcoin_cli -regtest getnewaddress)
  retry 20 1 signer_unconfirmed_balance_is "2.00000000"
  signer_unconfirmed_balance_is "2.00000000" || exit 1

  bitcoin_signer_cli_send_all_utxos \
    2.1 \
    0.38 \
    ${bitcoind_address}

  retry 60 1 wallet_pending_outgoing_is 210000000
  wallet_pending_outgoing_is 210000000 || exit 1
  retry 60 1 wallet_effective_settled_is_not 0
  wallet_effective_settled_is_not 0 || exit 1

  bitcoin_cli -generate 2
  retry 60 1 wallet_pending_outgoing_is 0
  wallet_pending_outgoing_is 0 || exit 1

  retry 60 1 wallet_effective_settled_matches_signer_balance
  wallet_effective_settled_matches_signer_balance || exit 1
}

@test "bitcoind_signer_sync: Can sweep all" {
  cache_wallet_balance
  [[ $(cached_current_settled) != 0 ]] || exit 1

  bitcoind_address=$(bitcoin_cli -regtest getnewaddress)
  bitcoin_signer_cli -named sendall recipients="[\"${bitcoind_address}\"]" fee_rate=1
  retry 60 1 wallet_current_settled_is_zero_and_pending_outgoing_is_not_zero
  wallet_current_settled_is_zero_and_pending_outgoing_is_not_zero || exit 1

  bitcoin_cli -generate 1
  retry 60 1 wallet_pending_outgoing_and_encumbered_fees_are_zero
  wallet_pending_outgoing_and_encumbered_fees_are_zero || exit 1
  retry 60 1 wallet_effective_settled_is 0
  wallet_effective_settled_is 0 || exit 1
}

@test "bitcoind_signer_sync: Can spend only from unconfirmed" {
  bitcoind_signer_address=$(bitcoin_signer_cli getnewaddress)
  bitcoin_cli -regtest sendtoaddress ${bitcoind_signer_address} 1
  retry 20 1 signer_unconfirmed_balance_is "1.00000000"
  signer_unconfirmed_balance_is "1.00000000" || exit 1

  bitcoind_address=$(bitcoin_cli -regtest getnewaddress)
  bitcoin_signer_cli_send_all_utxos \
    0.6 \
    0.39 \
    ${bitcoind_address}

  retry 60 1 wallet_pending_outgoing_is 60000000
  wallet_pending_outgoing_is 60000000 || exit 1
  retry 60 1 wallet_effective_settled_is 0
  wallet_effective_settled_is 0 || exit 1

  bitcoin_cli -generate 2
  retry 60 1 wallet_pending_outgoing_is 0
  wallet_pending_outgoing_is 0 || exit 1
  retry 60 1 wallet_effective_settled_matches_current_settled
  wallet_effective_settled_matches_current_settled || exit 1
  retry 60 1 wallet_effective_settled_matches_signer_balance
  wallet_effective_settled_matches_signer_balance || exit 1
}

@test "bitcoind_signer_sync: Batch broadcast ledger marker is set even when spend inputs are missing in bria_utxos" {
  cache_wallet_balance
  initial_settled=$(cached_current_settled)
  fund_btc_each="1"
  fund_sats_each=$(convert_btc_to_sats "${fund_btc_each}")
  expected_funding_sats=$(( fund_sats_each * 2 ))
  target_settled=$(( initial_settled + expected_funding_sats ))

  bria_cmd set-signer-config \
    --xpub "68bfb290" bitcoind \
    --endpoint "${BITCOIND_SIGNER_ENDPOINT}" \
    --rpc-user "rpcuser" \
    --rpc-password "invalidpassword"

  bria_cmd create-payout-queue -n drift_manual -m true

  bria_address=$(bria_cmd new-address -w default | jq -r '.address')
  bitcoin_cli -regtest sendtoaddress "${bria_address}" "${fund_btc_each}"
  bitcoin_cli -regtest sendtoaddress "${bria_address}" "${fund_btc_each}"
  bitcoin_cli -generate 6

  retry 60 1 wallet_current_settled_ge ${target_settled}
  wallet_current_settled_ge ${target_settled} || exit 1

  funded_delta=$(( $(cached_current_settled) - initial_settled ))
  [[ ${funded_delta} -ge ${expected_funding_sats} ]] || exit 1
  payout_amount=$(( funded_delta * 60 / 100 ))
  [[ ${payout_amount} -gt 0 ]] || exit 1

  payout_id=$(bria_cmd submit-payout -w default --queue-name drift_manual --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej --amount "${payout_amount}" | jq -r '.id')
  [[ "${payout_id}" != "null" ]] || exit 1

  for i in {1..40}; do
    bria_cmd trigger-payout-queue --name drift_manual
    batch_id=$(bria_cmd get-payout -i "${payout_id}" | jq -r '.payout.batchId')
    [[ "${batch_id}" != "null" ]] && break
    sleep 1
  done
  [[ "${batch_id}" != "null" ]] || exit 1

  reserved_outpoint=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT tx_id || ':' || vout FROM bria_utxos WHERE spending_batch_id = '${batch_id}' LIMIT 1" | tr -d '[:space:]')
  [[ -n "${reserved_outpoint}" ]] || exit 1

  reserved_txid=${reserved_outpoint%:*}
  reserved_vout=${reserved_outpoint#*:}
  docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -c "DELETE FROM bria_utxos WHERE tx_id = '${reserved_txid}' AND vout = ${reserved_vout}" > /dev/null
  bria_utxo_exists=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT COUNT(*) FROM bria_utxos WHERE tx_id = '${reserved_txid}' AND vout = ${reserved_vout}" | tr -d '[:space:]')
  [[ "${bria_utxo_exists}" -eq 0 ]] || exit 1

  bdk_copy_exists=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT COUNT(*) FROM bdk_utxos WHERE tx_id = '${reserved_txid}' AND vout = ${reserved_vout}" | tr -d '[:space:]')
  [[ "${bdk_copy_exists}" -ge 1 ]] || exit 1

  bria_cmd set-signer-config \
    --xpub "68bfb290" bitcoind \
    --endpoint "${BITCOIND_SIGNER_ENDPOINT}" \
    --rpc-user "rpcuser" \
    --rpc-password "rpcpassword"

  for i in {1..40}; do
    payout_tx_id=$(bria_cmd get-payout -i "${payout_id}" | jq -r '.payout.txId')
    [[ "${payout_tx_id}" != "null" ]] && break
    sleep 1
  done
  [[ "${payout_tx_id}" != "null" ]] || exit 1

  retry 60 1 bdk_tx_synced_flag_is "${payout_tx_id}" 0
  bdk_tx_synced_flag_is "${payout_tx_id}" 0 || exit 1

  for i in {1..60}; do
    broadcast_ledger_id=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT batch_broadcast_ledger_tx_id::text FROM bria_batch_wallet_summaries WHERE batch_id = '${batch_id}' LIMIT 1" | tr -d '[:space:]')
    [[ -n "${broadcast_ledger_id}" && "${broadcast_ledger_id}" != "null" ]] && break
    sleep 1
  done
  [[ -n "${broadcast_ledger_id}" && "${broadcast_ledger_id}" != "null" ]] || exit 1

  for i in {1..60}; do
    grep -q "spend_inputs_missing.*\"tx_id\":\"${payout_tx_id}\"" .e2e-logs && break
    sleep 1
  done
  grep -q "spend_inputs_missing.*\"tx_id\":\"${payout_tx_id}\"" .e2e-logs || exit 1
}

@test "bitcoind_signer_sync: AlreadyApplied spend path does not persist conflicting change state" {
  cache_wallet_balance
  initial_settled=$(cached_current_settled)
  fund_btc_each="1"
  fund_sats_each=$(convert_btc_to_sats "${fund_btc_each}")
  expected_funding_sats=$(( fund_sats_each * 2 ))
  target_settled=$(( initial_settled + expected_funding_sats ))
  queue_name="already_applied_manual_$RANDOM"

  bria_cmd set-signer-config \
    --xpub "68bfb290" bitcoind \
    --endpoint "${BITCOIND_SIGNER_ENDPOINT}" \
    --rpc-user "rpcuser" \
    --rpc-password "invalidpassword"

  bria_cmd create-payout-queue -n "${queue_name}" -m true

  bria_address=$(bria_cmd new-address -w default | jq -r '.address')
  bitcoin_cli -regtest sendtoaddress "${bria_address}" "${fund_btc_each}"
  bitcoin_cli -regtest sendtoaddress "${bria_address}" "${fund_btc_each}"
  bitcoin_cli -generate 6

  retry 60 1 wallet_current_settled_ge ${target_settled}
  wallet_current_settled_ge ${target_settled} || exit 1

  funded_delta=$(( $(cached_current_settled) - initial_settled ))
  [[ ${funded_delta} -ge ${expected_funding_sats} ]] || exit 1
  payout_amount=$(( funded_delta * 60 / 100 ))
  [[ ${payout_amount} -gt 0 ]] || exit 1

  payout_id=$(bria_cmd submit-payout -w default --queue-name "${queue_name}" --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej --amount "${payout_amount}" | jq -r '.id')
  [[ "${payout_id}" != "null" ]] || exit 1

  for i in {1..40}; do
    bria_cmd trigger-payout-queue --name "${queue_name}"
    batch_id=$(bria_cmd get-payout -i "${payout_id}" | jq -r '.payout.batchId')
    [[ "${batch_id}" != "null" ]] && break
    sleep 1
  done
  [[ "${batch_id}" != "null" ]] || exit 1

  reserved_count=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT COUNT(*) FROM bria_utxos WHERE spending_batch_id = '${batch_id}'" | tr -d '[:space:]')
  [[ "${reserved_count}" -ge 1 ]] || exit 1

  docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -c "UPDATE bria_utxos SET spend_detected_ledger_tx_id = gen_random_uuid(), bdk_spent = true WHERE spending_batch_id = '${batch_id}'" > /dev/null

  bria_cmd set-signer-config \
    --xpub "68bfb290" bitcoind \
    --endpoint "${BITCOIND_SIGNER_ENDPOINT}" \
    --rpc-user "rpcuser" \
    --rpc-password "rpcpassword"

  for i in {1..40}; do
    payout_tx_id=$(bria_cmd get-payout -i "${payout_id}" | jq -r '.payout.txId')
    [[ "${payout_tx_id}" != "null" ]] && break
    sleep 1
  done
  [[ "${payout_tx_id}" != "null" ]] || exit 1

  retry 180 1 bdk_tx_synced_flag_is "${payout_tx_id}" 1
  bdk_tx_synced_flag_is "${payout_tx_id}" 1 || exit 1

  for i in {1..60}; do
    broadcast_ledger_id=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT batch_broadcast_ledger_tx_id::text FROM bria_batch_wallet_summaries WHERE batch_id = '${batch_id}' LIMIT 1" | tr -d '[:space:]')
    [[ -n "${broadcast_ledger_id}" && "${broadcast_ledger_id}" != "null" ]] && break
    sleep 1
  done
  [[ -n "${broadcast_ledger_id}" && "${broadcast_ledger_id}" != "null" ]] || exit 1

  payout_utxo_count=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT COUNT(*) FROM bria_utxos WHERE tx_id = '${payout_tx_id}'" | tr -d '[:space:]')
  [[ "${payout_utxo_count}" -eq 0 ]] || exit 1

  payout_addr_event_count=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT COUNT(*) FROM bria_address_events WHERE event_type = 'metadata_updated' AND event->'metadata'->>'synced_in_tx' = '${payout_tx_id}'" | tr -d '[:space:]')
  [[ "${payout_addr_event_count}" -eq 0 ]] || exit 1

  ! grep -q "spend_inputs_missing.*\"tx_id\":\"${payout_tx_id}\"" .e2e-logs
}
