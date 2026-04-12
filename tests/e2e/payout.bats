#!/usr/bin/env bats

load "helpers"

setup_file() {
  e2e_init_file_context
  e2e_create_default_wallet_pair
  E2E_QUEUE_HIGH="$(e2e_queue_name high)"
  E2E_QUEUE_MANUAL="$(e2e_queue_name manual)"
  E2E_QUEUE_CANCEL="$(e2e_queue_name cancel_queue)"
  E2E_QUEUE_LARGE_TX="$(e2e_queue_name large_tx_queue)"
  E2E_QUEUE_STALE_SIGNER="$(e2e_queue_name stale_signer_queue)"
  E2E_OTHER_KEY="$(e2e_scoped_name other_key)"
  E2E_OTHER_WALLET="$(e2e_scoped_name other_wallet)"
  e2e_save_context
}

setup() {
  e2e_load_context
}

@test "payout: Batch inclusion and payout cancellation" {
  bria_cmd create-payout-queue --name "${E2E_QUEUE_HIGH}" --interval-trigger 5
  payout_id=$(bria_cmd submit-payout -w "${E2E_BRIA_WALLET}" --queue-name "${E2E_QUEUE_HIGH}" --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej --amount 75000000 | jq -r '.id')
  retry 60 1 wallet_encumbered_outgoing_is 75000000 "${E2E_BRIA_WALLET}"
  wallet_encumbered_outgoing_is 75000000 "${E2E_BRIA_WALLET}" || exit 1

  estimated_at=$(bria_cmd get-payout --id ${payout_id} | jq -r '.payout.batchInclusionEstimatedAt')
  [[ "${estimated_at}" != "null" ]] || exit 1

  bria_cmd cancel-payout --id ${payout_id}

  estimated_at=$(bria_cmd get-payout --id ${payout_id} | jq -r '.payout.batchInclusionEstimatedAt')
  [[ "${estimated_at}" = "null" ]] || exit 1

  retry 60 1 wallet_encumbered_outgoing_is 0 "${E2E_BRIA_WALLET}"
  wallet_encumbered_outgoing_is 0 "${E2E_BRIA_WALLET}" || exit 1
}

@test "payout: Fund an address and see if the balance is reflected" {
  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')
  if [ -z "$bria_address" ]; then
    echo "Failed to get a new address"
    exit 1
  fi

  external_wallet_send_to_address "${bria_address}" 1
  external_wallet_send_to_address "${bria_address}" 1

  for i in {1..60}; do
   n_utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}" | jq '.keychains[0].utxos | length')
    [[ "${n_utxos}" == "3" ]] && break
    sleep 1
  done
  cache_wallet_balance "${E2E_BRIA_WALLET}"
  [[ $(cached_encumbered_fees) != 0 ]] || exit 1
  [[ $(cached_pending_income) == 200000000 ]] || exit 1;
}

@test "payout: Create payout queue and have two queued payouts on it" {
  bria_cmd submit-payout --wallet "${E2E_BRIA_WALLET}" --queue-name "${E2E_QUEUE_HIGH}" --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej --amount 75000000
  bria_cmd submit-payout --wallet "${E2E_BRIA_WALLET}" --queue-name "${E2E_QUEUE_HIGH}" --destination bcrt1q3rr02wkkvkwcj7h0nr9dqr9z3z3066pktat7kv --amount 75000000 --metadata '{"foo":{"bar":"baz"}}'

  n_payouts=$(bria_cmd list-payouts -w "${E2E_BRIA_WALLET}" | jq '.payouts | length')
  [[ "${n_payouts}" == "3" ]] || exit 1
  batch_id=$(bria_cmd list-payouts -w "${E2E_BRIA_WALLET}" | jq '.payouts[0].batchId')
  [[ "${batch_id}" == "null" ]] || exit 1
  cache_wallet_balance "${E2E_BRIA_WALLET}"
  [[ $(cached_encumbered_outgoing) == 150000000 && $(cached_pending_outgoing) == 0 ]] || exit 1
}

@test "payout: Settling income means batch is created" {
  bitcoin_cli -generate 20

  for i in {1..60}; do
    utxo_height=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}" | jq '.keychains[0].utxos[0].blockHeight')
    [[ "${utxo_height}" != "null" ]] && break;
    sleep 1
  done
  cache_wallet_balance "${E2E_BRIA_WALLET}"
  [[ $(cached_pending_income) == 0 ]] || exit 1

  for i in {1..20}; do
    batch_id=$(bria_cmd list-payouts -w "${E2E_BRIA_WALLET}" | jq -r '.payouts[1].batchId')
    [[ "${batch_id}" != "null" ]] && break
    sleep 1
  done
  [[ "${batch_id}" != "null" ]] || exit 1
  retry 60 1 wallet_pending_outgoing_is 150000000 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 150000000 "${E2E_BRIA_WALLET}" || exit 1
  [[ $(cached_pending_fees) != 0 ]] || exit 1
  [[ $(cached_encumbered_fees) == 0 ]] || exit 1
}

@test "payout: Add signing config to complete payout" {
  batch_id=$(bria_cmd list-payouts -w "${E2E_BRIA_WALLET}" | jq -r '.payouts[1].batchId')
  for i in {1..20}; do
    signing_failure_reason=$(bria_cmd get-batch -b "${batch_id}" | jq -r '.signingSessions[0].failureReason')
    [[ "${signing_failure_reason}" == "SignerConfigMissing" ]] && break
    sleep 1
  done

  [[ "${signing_failure_reason}" == "SignerConfigMissing" ]] || exit 1

  cache_wallet_balance "${E2E_BRIA_WALLET}"
  [[ $(cached_pending_income) == 0 ]] || exit 1

  e2e_ensure_default_signer_wallet_loaded
  bria_cmd set-signer-config \
    --xpub "${E2E_SIGNER_XPUB_REF}" bitcoind \
    --endpoint "$(e2e_bitcoind_signer_endpoint)" \
    --rpc-user "rpcuser" \
    --rpc-password "rpcpassword"

  for i in {1..20}; do
    signing_status=$(bria_cmd get-batch -b "${batch_id}" | jq -r '.signingSessions[0].state')
    [[ "${signing_status}" == "Complete" ]] && break
    sleep 1
  done
  if [[ "${signing_status}" != "Complete" ]]; then
    signing_failure_reason=$(bria_cmd get-batch -b "${batch_id}" | jq -r '.signingSessions[0].failureReason')
    echo "signing_status: ${signing_status}"
    echo "signing_failure_reason: ${signing_failure_reason}"
  fi

  retry 60 1 wallet_pending_income_is_not 0 "${E2E_BRIA_WALLET}"
  wallet_pending_income_is_not 0 "${E2E_BRIA_WALLET}" || exit 1
  [[ $(cached_current_settled) == 0 ]] || exit 1
  bitcoin_cli -generate 2

  retry 60 1 wallet_current_settled_or_pending_outgoing_is_not_zero "${E2E_BRIA_WALLET}"
  wallet_current_settled_or_pending_outgoing_is_not_zero "${E2E_BRIA_WALLET}" || exit 1
}

@test "payout: Creates a manually triggered payout-queue and triggers it" {
  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')
  external_wallet_send_to_address "${bria_address}" 1
  bitcoin_cli -generate 10
  bria_cmd create-payout-queue -n "${E2E_QUEUE_MANUAL}" -m true
  bria_cmd submit-payout --wallet "${E2E_BRIA_WALLET}" --queue-name "${E2E_QUEUE_MANUAL}" --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej --amount 75000000

  for i in {1..20}; do
    batch_id=$(bria_cmd list-payouts -w "${E2E_BRIA_WALLET}" | jq -r '.payouts[0].batchId')
     [[ "${batch_id}" != "null" ]] && break;
    sleep 1
  done
  [[ "${batch_id}" == "null" ]] || exit 1

  bria_cmd trigger-payout-queue --name "${E2E_QUEUE_MANUAL}";

  for i in {1..20}; do
    payout=$(bria_cmd list-payouts -w "${E2E_BRIA_WALLET}" | jq -r '.payouts[0]')
    payout_id=$(echo ${payout} | jq -r '.id')
    batch_id=$(echo ${payout} | jq -r '.batchId')
    tx_id=$(echo ${payout} | jq -r '.txId')
    vout=$(echo ${payout} | jq -r '.vout')
    [[ "${batch_id}" != "null" && "${tx_id}" != "null" && "${vout}" != "null" ]] && break
    sleep 1
  done
  [[ "${batch_id}" != "null" && "${tx_id}" != "null" && "${vout}" != "null" ]] || exit 1

  payout=$(bria_cmd get-payout --id ${payout_id} | jq -r '.payout')
  batch_id=$(echo ${payout} | jq -r '.batchId')
  tx_id=$(echo ${payout} | jq -r '.txId')
  vout=$(echo ${payout} | jq -r '.vout')
  [[ "${batch_id}" != "null" && "${tx_id}" != "null" && "${vout}" != "null" ]] || exit 1

  retry 60 1 wallet_pending_income_is_not 0 "${E2E_BRIA_WALLET}"
  wallet_pending_income_is_not 0 "${E2E_BRIA_WALLET}" || exit 1

  bitcoin_cli -generate 2

  retry 60 1 wallet_pending_income_is 0 "${E2E_BRIA_WALLET}"
  wallet_pending_income_is 0 "${E2E_BRIA_WALLET}" || exit 1
}

@test "payout: Can send to another wallet" {
  local key="tpubDEPCxBfMFRNdfJaUeoTmepLJ6ZQmeTiU1Sko2sdx1R3tmPpZemRUjdAHqtmLfaVrBg1NBx2Yx3cVrsZ2FTyBuhiH9mPSL5ozkaTh1iZUTZx"
  local other_xpub_id

  other_xpub_id=$(bria_cmd list-xpubs | jq -r --arg xpub "${key}" '([.xpubs[] | select(.xpub == $xpub) | .id][0]) // ""')
  if [[ -z "${other_xpub_id}" ]]; then
    bria_cmd import-xpub -x "${key}" -n "${E2E_OTHER_KEY}" -d m/48h/1h/0h/2h
    other_xpub_id="${E2E_OTHER_KEY}"
  fi
  bria_cmd create-wallet -n "${E2E_OTHER_WALLET}" wpkh -x "${other_xpub_id}"

  bria_cmd submit-payout -w "${E2E_BRIA_WALLET}" \
    --queue-name "${E2E_QUEUE_HIGH}" \
    --destination "${E2E_OTHER_WALLET}" \
    --amount 70000000 \
    --metadata '{"transfer":true}'

  transfer_metadata=$(bria_cmd list-addresses -w "${E2E_OTHER_WALLET}" | jq -r '.addresses[0].metadata.transfer')

  [[ "${transfer_metadata}" == "true" ]] || exit 1

  retry 60 1 wallet_pending_outgoing_is 70000000 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 70000000 "${E2E_BRIA_WALLET}" || exit 1
}

@test "payout: Can CPFP when enabled in payout queue" {
  for i in {1..20}; do
    available_utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}" | jq -r '.keychains[0].utxos | length')
    [[ "${available_utxos}" == "1" ]] && break
    sleep 1
  done
  [[ "${available_utxos}" == "1" ]] || exit 1

  for i in {1..20}; do
    block_height=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}" | jq -r '.keychains[0].utxos[0].blockHeight')
    [[ "${block_height}" == "null" ]] && break
    sleep 1
  done
  [[ "${block_height}" == "null" ]] || exit 1

  bria_cmd submit-payout -w "${E2E_BRIA_WALLET}" \
    --queue-name "${E2E_QUEUE_HIGH}" \
    --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej \
    --amount 100000

  retry 60 1 wallet_encumbered_outgoing_is 100000 "${E2E_BRIA_WALLET}"
  wallet_encumbered_outgoing_is 100000 "${E2E_BRIA_WALLET}" || exit 1

  batch_id=$(bria_cmd list-payouts -w "${E2E_BRIA_WALLET}" | jq -r '.payouts[0].batchId')
  [[ "${batch_id}" == "null" ]] || exit 1

  queue_id=$(bria_cmd list-payout-queues | jq -r --arg queue_name "${E2E_QUEUE_HIGH}" '.PayoutQueues[] | select(.name == $queue_name).id')
  bria_cmd update-payout-queue -i "${queue_id}" --interval-trigger 5 --cpfp-after-mins 0

  for i in {1..90}; do
    batch_id=$(bria_cmd list-payouts -w "${E2E_BRIA_WALLET}" | jq -r '.payouts[0].batchId')
    [[ "${batch_id}" != "null" ]] && break
    sleep 1
  done
  [[ "${batch_id}" != "null" ]] || exit 1;

  retry 60 1 wallet_encumbered_outgoing_is_zero "${E2E_BRIA_WALLET}"
  wallet_encumbered_outgoing_is_zero "${E2E_BRIA_WALLET}" || exit 1
}

@test "payout: Create and cancel an unsigned batch" {
  # invalidates signer to allow cancel the batch
  e2e_ensure_default_signer_wallet_loaded
  bria_cmd set-signer-config \
    --xpub "${E2E_SIGNER_XPUB_REF}" bitcoind \
    --endpoint "$(e2e_bitcoind_signer_endpoint)" \
    --rpc-user "rpcuser" \
    --rpc-password "invalidpassword"

  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')
  external_wallet_send_to_address "${bria_address}" 1
  bitcoin_cli -generate 10

  bria_cmd create-payout-queue -n "${E2E_QUEUE_CANCEL}" -m true
  payout_id=$(bria_cmd submit-payout -w "${E2E_BRIA_WALLET}" --queue-name "${E2E_QUEUE_CANCEL}" --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej --amount 1300000 | jq -r '.id')

  # Wait for payout to be encumbered
  retry 60 1 wallet_encumbered_outgoing_is_and_effective_settled_ge 1300000 100000000 "${E2E_BRIA_WALLET}"
  wallet_encumbered_outgoing_is_and_effective_settled_ge 1300000 100000000 "${E2E_BRIA_WALLET}" || exit 1
  effective_settled=$(cached_effective_settled)

  # Wait for the batch to be created
  for i in {1..20}; do
    bria_cmd trigger-payout-queue --name "${E2E_QUEUE_CANCEL}"
    batch_id=$(bria_cmd get-payout -i "${payout_id}" | jq -r '.payout.batchId')
    [[ "${batch_id}" != "null" ]] && break
    sleep 2
  done
  [[ "${batch_id}" != "null" ]] || exit 1

  # Verify the batch exists
  batch=$(bria_cmd get-batch -b "${batch_id}")
  [[ $(echo ${batch} | jq -r '.id') == "${batch_id}" && $(echo ${batch} | jq -r '.cancelled') == "false" ]] || exit 1

  # Capture at least one UTXO reserved for this batch before cancellation
  for i in {1..20}; do
    reserved_outpoint=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT tx_id || ':' || vout FROM bria_utxos WHERE spending_batch_id = '${batch_id}' LIMIT 1" | tr -d '[:space:]')
    [[ -n "${reserved_outpoint}" ]] && break
    sleep 1
  done
  [[ -n "${reserved_outpoint}" ]] || exit 1
  reserved_for_batch_before=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT COUNT(*) FROM bria_utxos WHERE spending_batch_id = '${batch_id}'" | tr -d '[:space:]')
  [[ "${reserved_for_batch_before}" -ge 1 ]] || exit 1

  # Cancel the batch
  bria_cmd cancel-batch --batch-id "${batch_id}"

  # Verify reservation fields were cleared for the cancelled batch
  reserved_for_batch_after=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT COUNT(*) FROM bria_utxos WHERE spending_batch_id = '${batch_id}'" | tr -d '[:space:]')
  [[ "${reserved_for_batch_after}" == "0" ]] || exit 1

  reserved_txid=${reserved_outpoint%:*}
  reserved_vout=${reserved_outpoint#*:}
  reserved_fields=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT (spending_batch_id IS NULL AND spending_payout_queue_id IS NULL AND spending_sats_per_vbyte IS NULL)::int FROM bria_utxos WHERE tx_id = '${reserved_txid}' AND vout = ${reserved_vout} LIMIT 1" | tr -d '[:space:]')
  [[ "${reserved_fields}" == "1" ]] || exit 1

  # Verify the payout is marked as cancelled
  for i in {1..20}; do
    payout=$(bria_cmd get-payout -i ${payout_id} | jq -r '.payout')
    batch_id_after=$(echo ${payout} | jq -r '.batchId')
    cancelled=$(echo ${payout} | jq -r '.cancelled')
    [[ "${batch_id_after}" == "${batch_id}" && "${cancelled}" == "true" ]] && break
    sleep 1
  done
  [[ "${batch_id_after}" == "${batch_id}" && "${cancelled}" == "true" ]] || exit 1

  # Verify the batch is marked as cancelled
  batch=$(bria_cmd get-batch -b "${batch_id}")
  [[ $(echo ${batch} | jq -r '.id') == "${batch_id}" && $(echo ${batch} | jq -r '.cancelled') == "true" ]] || exit 1

  # Check that the funds are no longer encumbered
  retry 60 1 wallet_encumbered_outgoing_is_and_effective_settled_is 0 ${effective_settled} "${E2E_BRIA_WALLET}"
  wallet_encumbered_outgoing_is_and_effective_settled_is 0 ${effective_settled} "${E2E_BRIA_WALLET}" || exit 1
}

@test "payout: Error when try to create and cancel a signed batch" {
  e2e_ensure_default_signer_wallet_loaded
  bria_cmd set-signer-config \
    --xpub "${E2E_SIGNER_XPUB_REF}" bitcoind \
    --endpoint "$(e2e_bitcoind_signer_endpoint)" \
    --rpc-user "rpcuser" \
    --rpc-password "rpcpassword"

  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')
  external_wallet_send_to_address "${bria_address}" 1
  bitcoin_cli -generate 10

  bria_cmd create-payout-queue -n "${E2E_QUEUE_CANCEL}" -m true || true
  payout_id=$(bria_cmd submit-payout -w "${E2E_BRIA_WALLET}" --queue-name "${E2E_QUEUE_CANCEL}" --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej --amount 1300000 | jq -r '.id')

  # Wait for payout to be encumbered
  retry 60 1 wallet_encumbered_outgoing_is_and_effective_settled_ge 1300000 100000000 "${E2E_BRIA_WALLET}"
  wallet_encumbered_outgoing_is_and_effective_settled_ge 1300000 100000000 "${E2E_BRIA_WALLET}" || exit 1

  # Wait for the batch to be created
  for i in {1..20}; do
    bria_cmd trigger-payout-queue --name "${E2E_QUEUE_CANCEL}"
    batch_id=$(bria_cmd get-payout -i "${payout_id}" | jq -r '.payout.batchId')
    [[ "${batch_id}" != "null" ]] && break
    sleep 2
  done
  [[ "${batch_id}" != "null" ]] || exit 1

  # Verify the batch exists
  batch=$(bria_cmd get-batch -b "${batch_id}")
  [[ $(echo ${batch} | jq -r '.id') == "${batch_id}" ]] || exit 1

  # Try to cancel the batch
  run bria_cmd cancel-batch --batch-id "${batch_id}"
  [[ "$status" -ne 0 ]]
  [[ "$output" == *"BatchError - Batch is already signed"* ]]

  # Check that the funds are no longer encumbered
  retry 60 1 wallet_encumbered_outgoing_is 0 "${E2E_BRIA_WALLET}"
  wallet_encumbered_outgoing_is 0 "${E2E_BRIA_WALLET}" || exit 1

  # Verify the batch is not marked as cancelled
  batch=$(bria_cmd get-batch -b "${batch_id}")
  [[ $(echo ${batch} | jq -r '.id') == "${batch_id}" && $(echo ${batch} | jq -r '.cancelled') == "false" ]] || exit 1
}

@test "payout: Estimate payout fee returns positive fee and fee_rate" {
  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')
  [[ -n "${bria_address}" ]] || exit 1

  response=$(bria_cmd estimate-payout-fee \
    --wallet "${E2E_BRIA_WALLET}" \
    --queue-name "${E2E_QUEUE_HIGH}" \
    --destination "${bria_address}" \
    --amount 10000)

  estimated_fee=$(echo "${response}" | jq -r '.satoshis')
  fee_rate=$(echo "${response}" | jq -r '.feeRate')

  [[ "${estimated_fee}" =~ ^[0-9]+$ ]] || exit 1
  [[ "${estimated_fee}" -gt 0 ]] || exit 1

  [[ "${fee_rate}" =~ ^[0-9]+(\.[0-9]+)?$ ]] || exit 1
  [[ "$(echo "${fee_rate} > 0" | bc -l)" -eq 1 ]] || exit 1
}

@test "payout: Can sync transaction with 120+ inputs without payload error" {
  bitcoin_cli -generate 6

  bitcoind_signer_address=$(bitcoin_signer_cli getnewaddress)
  if [ -z "$bitcoind_signer_address" ]; then
    echo "Failed to get a new address"
    exit 1
  fi

  echo "Creating 130 UTXOs..."
  for i in {1..130}; do
    external_wallet_send_to_address "${bitcoind_signer_address}" 0.01
  done

  for i in {1..60}; do
    n_utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}" | jq '.keychains[0].utxos | length')
    echo "Detected UTXOs: ${n_utxos}"
    [[ "${n_utxos}" -ge "130" ]] && break
    sleep 1
  done
  n_utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}" | jq '.keychains[0].utxos | length')
  [[ "${n_utxos}" -ge "130" ]] || exit 1

  bitcoin_cli -generate 6

  retry 60 1 wallet_current_settled_ge 130000000 "${E2E_BRIA_WALLET}"
  wallet_current_settled_ge 130000000 "${E2E_BRIA_WALLET}" || exit 1

  echo "Creating transaction with 130+ inputs..."
  bitcoind_address=$(external_wallet_new_address)
  bitcoin_signer_cli -named sendall recipients="[\"${bitcoind_address}\"]" fee_rate=1

  echo "Waiting for spend to be detected..."
  retry 60 1 wallet_pending_outgoing_is_not 0 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is_not 0 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_current_settled_is 0 "${E2E_BRIA_WALLET}"
  wallet_current_settled_is 0 "${E2E_BRIA_WALLET}" || exit 1

  echo "Confirming the spending transaction..."
  bitcoin_cli -generate 6

  echo "Waiting for spend to be settled..."
  retry 120 1 wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}" || exit 1

  retry 60 1 wallet_current_settled_is 0 "${E2E_BRIA_WALLET}"
  wallet_current_settled_is 0 "${E2E_BRIA_WALLET}" || exit 1
}

@test "payout: Can create payout batch with 120+ inputs without payload error" {
  e2e_ensure_default_signer_wallet_loaded
  bria_cmd set-signer-config \
    --xpub "${E2E_SIGNER_XPUB_REF}" bitcoind \
    --endpoint "$(e2e_bitcoind_signer_endpoint)" \
    --rpc-user "rpcuser" \
    --rpc-password "rpcpassword"

  bria_cmd create-payout-queue --name "${E2E_QUEUE_LARGE_TX}" --interval-trigger 5

  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')
  if [ -z "$bria_address" ]; then
    echo "Failed to get a new address"
    exit 1
  fi

  echo "Creating 130 UTXOs for payout test..."
  for i in {1..130}; do
    external_wallet_send_to_address "${bria_address}" 0.01
  done

  for i in {1..60}; do
    n_utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}" | jq '.keychains[0].utxos | length')
    echo "Detected UTXOs: ${n_utxos}"
    [[ "${n_utxos}" == "130" ]] && break
    sleep 1
  done
  n_utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}" | jq '.keychains[0].utxos | length')
  [[ "${n_utxos}" == "130" ]] || exit 1

  bitcoin_cli -generate 6

  retry 60 1 wallet_current_settled_is 130000000 "${E2E_BRIA_WALLET}"
  wallet_current_settled_is 130000000 "${E2E_BRIA_WALLET}" || exit 1

  echo "Submitting payout that will use 130 inputs..."
  destination="bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej"
  payout_id=$(bria_cmd submit-payout -w "${E2E_BRIA_WALLET}" --queue-name "${E2E_QUEUE_LARGE_TX}" --destination ${destination} --amount 125000000 | jq -r '.id')

  retry 60 1 wallet_encumbered_outgoing_is 125000000 "${E2E_BRIA_WALLET}"
  wallet_encumbered_outgoing_is 125000000 "${E2E_BRIA_WALLET}" || exit 1

  echo "Waiting for batch creation and broadcast..."
  for i in {1..60}; do
    batch_id=$(bria_cmd get-payout --id ${payout_id} | jq -r '.payout.batchId')
    echo "Batch ID: ${batch_id}"
    [[ "${batch_id}" != "null" ]] && break
    sleep 1
  done
  batch_id=$(bria_cmd get-payout --id ${payout_id} | jq -r '.payout.batchId')
  [[ "${batch_id}" != "null" ]] || exit 1

  echo "Waiting for spend to be detected..."
  retry 60 1 wallet_pending_outgoing_is 125000000 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 125000000 "${E2E_BRIA_WALLET}" || exit 1
  [[ $(cached_encumbered_outgoing) == "0" ]] || exit 1

  echo "Confirming the batch transaction..."
  bitcoin_cli -generate 6

  echo "Waiting for batch to be settled..."
  retry 60 1 wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}" || exit 1

  cache_wallet_balance "${E2E_BRIA_WALLET}"
  settled=$(cached_current_settled)
  [[ "${settled}" -gt "0" && "${settled}" -le "5000000" ]] || exit 1
}

@test "payout: Batch is not marked signed when bitcoind signer index is stale" {
  bitcoin_cli -generate 6
  bitcoin_signer_cli -named sendall recipients="[\"bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej\"]" fee_rate=1 || true
  bitcoin_cli -generate 6

  e2e_ensure_default_signer_wallet_loaded
  bria_cmd set-signer-config \
    --xpub "${E2E_SIGNER_XPUB_REF}" bitcoind \
    --endpoint "$(e2e_bitcoind_signer_endpoint)" \
    --rpc-user "rpcuser" \
    --rpc-password "rpcpassword"

  bria_cmd create-payout-queue -n "${E2E_QUEUE_STALE_SIGNER}" -m true || true

  # Advance Bria's address index until we hit an address that bitcoind signer
  # does not recognize in its imported descriptor range.
  stale_address=""
  signer_knows_address="true"
  for i in {1..3000}; do
    stale_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')
    if (( i % 150 == 0 )); then
      signer_knows_address=$(bitcoin_signer_cli getaddressinfo "${stale_address}" | jq -r '.ismine')
      [[ "${signer_knows_address}" == "false" ]] && break
    fi
  done
  if [[ "${signer_knows_address}" != "false" ]]; then
    signer_knows_address=$(bitcoin_signer_cli getaddressinfo "${stale_address}" | jq -r '.ismine')
  fi
  [[ -n "${stale_address}" ]] || exit 1
  [[ "${signer_knows_address}" == "false" ]] || exit 1

  external_wallet_send_to_address "${stale_address}" 1
  bitcoin_cli -generate 6

  retry 60 1 wallet_current_settled_ge 100000000 "${E2E_BRIA_WALLET}"
  wallet_current_settled_ge 100000000 "${E2E_BRIA_WALLET}" || exit 1

  payout_id=$(bria_cmd submit-payout -w "${E2E_BRIA_WALLET}" --queue-name "${E2E_QUEUE_STALE_SIGNER}" --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej --amount 99000000 | jq -r '.id')
  [[ "${payout_id}" != "null" ]] || exit 1

  for i in {1..20}; do
    bria_cmd trigger-payout-queue --name "${E2E_QUEUE_STALE_SIGNER}"
    batch_id=$(bria_cmd get-payout -i "${payout_id}" | jq -r '.payout.batchId')
    [[ "${batch_id}" != "null" ]] && break
    sleep 2
  done
  [[ "${batch_id}" != "null" ]] || exit 1

  for i in {1..60}; do
    batch=$(bria_cmd get-batch -b "${batch_id}")
    signing_sessions_count=$(echo ${batch} | jq -r '.signingSessions | length')
    signing_state=$(echo ${batch} | jq -r '.signingSessions[0].state')
    signing_failure_reason=$(echo ${batch} | jq -r '.signingSessions[0].failureReason')
    [[ "${signing_sessions_count}" -ge 1 ]] || { sleep 1; continue; }
    [[ "${signing_state}" == "Failed" ]] && break
    [[ "${signing_failure_reason}" != "null" ]] && break
    sleep 1
  done

  echo "${batch_id}"
  [[ "${signing_state}" != "Complete" ]] || exit 1
  [[ "${signing_failure_reason}" == "WalletError - Submitted Psbt does not have valid signatures." ]] || exit 1

  bria_cmd cancel-batch --batch-id "${batch_id}"

  batch=$(bria_cmd get-batch -b "${batch_id}")
  [[ $(echo ${batch} | jq -r '.cancelled') == "true" ]] || exit 1

  # Re-sync signer index so subsequent tests start from a clean state.
  signer_knows_address=$(bitcoin_signer_cli getaddressinfo "${stale_address}" | jq -r '.ismine')
  if [[ "${signer_knows_address}" != "true" ]]; then
    for i in {1..3000}; do
      bitcoin_signer_cli getnewaddress > /dev/null
      if (( i % 50 == 0 )); then
        signer_knows_address=$(bitcoin_signer_cli getaddressinfo "${stale_address}" | jq -r '.ismine')
        [[ "${signer_knows_address}" == "true" ]] && break
      fi
    done
    if [[ "${signer_knows_address}" != "true" ]]; then
      signer_knows_address=$(bitcoin_signer_cli getaddressinfo "${stale_address}" | jq -r '.ismine')
    fi
  fi
  [[ "${signer_knows_address}" == "true" ]] || exit 1
}
