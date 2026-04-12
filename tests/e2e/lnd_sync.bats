#!/usr/bin/env bats

load "helpers"

setup_file() {
  e2e_init_file_context
  e2e_create_lnd_wallet
  e2e_save_context
}

setup() {
  e2e_load_context
}

@test "lnd_sync: Generates the same address" {
  lnd_address=$(lnd_cli newaddress p2wkh | jq -r '.address')
  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')

  [ "$lnd_address" = "$bria_address" ]

  n_addresses=$(bria_cmd list-addresses -w "${E2E_BRIA_WALLET}" | jq -r '.addresses | length')
  [ "$n_addresses" = "1" ] || exit 1
}

@test "lnd_sync: Detects incoming transactions" {
  lnd_address=$(lnd_cli newaddress p2wkh | jq -r '.address')
  if [ -z "$lnd_address" ]; then
    echo "Failed to get a new address"
    exit 1
  fi

  external_wallet_send_to_address "${lnd_address}" 1

  retry 60 1 wallet_pending_income_is 100000000 "${E2E_BRIA_WALLET}"
  wallet_pending_income_is 100000000 "${E2E_BRIA_WALLET}" || exit 1

  n_addresses=$(bria_cmd list-addresses -w "${E2E_BRIA_WALLET}" | jq -r '.addresses | length')
  [ "$n_addresses" = "2" ] || exit 1
  utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}")
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  utxo_block_height=$(jq -r '.keychains[0].utxos[0].blockHeight' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${utxo_block_height}" == "null" ]]

  bitcoin_cli -generate 2

  retry 60 1 wallet_current_settled_is 100000000 "${E2E_BRIA_WALLET}"
  wallet_current_settled_is 100000000 "${E2E_BRIA_WALLET}" || exit 1

  utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}")
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  utxo_block_height=$(jq -r '.keychains[0].utxos[0].blockHeight' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${utxo_block_height}" != "null" ]]
}

@test "lnd_sync: Detects outgoing transactions" {
  bitcoind_address=$(external_wallet_new_address)
  txid=$(lnd_cli sendcoins --addr=${bitcoind_address} --amt=50000000 | jq -r '.txid')
  [[ -n "${txid}" && "${txid}" != "null" ]] || exit 1
  retry 20 1 bitcoin_mempool_has_tx "${txid}"
  bitcoin_mempool_has_tx "${txid}" || exit 1
  retry 60 1 wallet_pending_outgoing_is 50000000 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 50000000 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_current_settled_is 0 "${E2E_BRIA_WALLET}"
  wallet_current_settled_is 0 "${E2E_BRIA_WALLET}" || exit 1

  utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}")
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  change=$(jq -r '.keychains[0].utxos[0].changeOutput' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${change}" == "true" ]]

  bitcoin_cli -generate 1

  retry 60 1 wallet_current_settled_is_not 0 "${E2E_BRIA_WALLET}"
  wallet_current_settled_is_not 0 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}" || exit 1

  utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}")
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  utxo_block_height=$(jq -r '.keychains[0].utxos[0].blockHeight' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${utxo_block_height}" != "null" ]]
}

@test "lnd_sync: Can handle spend from mix of unconfirmed UTXOs" {
  lnd_address=$(lnd_cli newaddress p2wkh | jq -r '.address')
  if [ -z "$lnd_address" ]; then
    echo "Failed to get a new address"
    exit 1
  fi

  external_wallet_send_to_address "${lnd_address}" 1
  external_wallet_send_to_address "${lnd_address}" 1

  retry 60 1 wallet_pending_income_is_not 0 "${E2E_BRIA_WALLET}"
  wallet_pending_income_is_not 0 "${E2E_BRIA_WALLET}" || exit 1

  bitcoind_address=$(external_wallet_new_address)
  retry 20 1 lnd_unconfirmed_balance_is 200000000
  lnd_unconfirmed_balance_is 200000000 || exit 1
  txid=$(lnd_cli sendcoins --addr=${bitcoind_address} --amt=210000000 --min_confs 0 | jq -r '.txid')
  [[ -n "${txid}" && "${txid}" != "null" ]] || exit 1
  retry 20 1 bitcoin_mempool_has_tx "${txid}"
  bitcoin_mempool_has_tx "${txid}" || exit 1

  retry 60 1 wallet_pending_outgoing_is 210000000 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 210000000 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_effective_settled_is_not 0 "${E2E_BRIA_WALLET}"
  wallet_effective_settled_is_not 0 "${E2E_BRIA_WALLET}" || exit 1

  bitcoin_cli -generate 2
  retry 60 1 wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}" || exit 1

  retry 60 1 wallet_effective_settled_matches_lnd_balance "${E2E_BRIA_WALLET}"
  wallet_effective_settled_matches_lnd_balance "${E2E_BRIA_WALLET}" || exit 1
}

@test "lnd_sync: Can sweep all" {
  bitcoind_address=$(external_wallet_new_address)
  lnd_cli sendcoins --addr=${bitcoind_address} --sweepall
  bitcoin_cli -generate 1

  retry 60 1 wallet_encumbered_fees_is 0 "${E2E_BRIA_WALLET}"
  wallet_encumbered_fees_is 0 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_effective_settled_is 0 "${E2E_BRIA_WALLET}"
  wallet_effective_settled_is 0 "${E2E_BRIA_WALLET}" || exit 1
}

@test "lnd_sync: Can spend only from unconfirmed" {
  lnd_address=$(lnd_cli newaddress p2wkh | jq -r '.address')
  external_wallet_send_to_address "${lnd_address}" 1

  retry 60 1 wallet_pending_income_is 100000000 "${E2E_BRIA_WALLET}"
  wallet_pending_income_is 100000000 "${E2E_BRIA_WALLET}" || exit 1

  retry 20 1 lnd_unconfirmed_balance_is 100000000
  lnd_unconfirmed_balance_is 100000000 || exit 1

  bitcoind_address=$(external_wallet_new_address)
  txid=$(lnd_cli sendcoins --addr=${bitcoind_address} --amt=60000000 --min_confs 0 | jq -r '.txid')
  [[ -n "${txid}" && "${txid}" != "null" ]] || exit 1
  retry 20 1 bitcoin_mempool_has_tx "${txid}"
  bitcoin_mempool_has_tx "${txid}" || exit 1

  retry 60 1 wallet_pending_outgoing_is 60000000 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 60000000 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_effective_settled_is 0 "${E2E_BRIA_WALLET}"
  wallet_effective_settled_is 0 "${E2E_BRIA_WALLET}" || exit 1

  bitcoin_cli -generate 2
  retry 60 1 wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_effective_settled_matches_current_settled "${E2E_BRIA_WALLET}"
  wallet_effective_settled_matches_current_settled "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_effective_settled_matches_lnd_balance "${E2E_BRIA_WALLET}"
  wallet_effective_settled_matches_lnd_balance "${E2E_BRIA_WALLET}" || exit 1
}
