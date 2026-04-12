#!/usr/bin/env bats

load "helpers"

setup_file() {
  e2e_init_file_context
  e2e_create_default_wallet_pair
  e2e_save_context
}

setup() {
  e2e_load_context
}

@test "bitcoind_signer_sync: Generates the same address" {
  bitcoind_signer_address=$(bitcoin_signer_cli getnewaddress)
  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')

  [ "$bitcoind_signer_address" = "$bria_address" ] || exit 1

  n_addresses=$(bria_cmd list-addresses -w "${E2E_BRIA_WALLET}" | jq -r '.addresses | length')
  [ "$n_addresses" = "1" ] || exit 1
}

@test "bitcoind_signer_sync: Detects incoming transactions" {
  bitcoind_signer_address=$(bitcoin_signer_cli getnewaddress)
  if [ -z "$bitcoind_signer_address" ]; then
    echo "Failed to get a new address"
    exit 1
  fi

  external_wallet_send_to_address "${bitcoind_signer_address}" 1

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

  [[ "${n_utxos}" == "1" && "${utxo_block_height}" == "201" ]] || exit 1
}

@test "bitcoind_signer_sync: Detects outgoing transactions" {
  bitcoind_address=$(external_wallet_new_address)
  bitcoin_signer_cli -regtest sendtoaddress "${bitcoind_address}" 0.5
  retry 60 1 wallet_pending_outgoing_is 50000000 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 50000000 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_current_settled_is 0 "${E2E_BRIA_WALLET}"
  wallet_current_settled_is 0 "${E2E_BRIA_WALLET}" || exit 1

  utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}")
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  change=$(jq -r '.keychains[0].utxos[0].changeOutput' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${change}" == "true" ]] || exit 1

  bitcoin_cli -generate 1

  retry 60 1 wallet_current_settled_is_not 0 "${E2E_BRIA_WALLET}"
  wallet_current_settled_is_not 0 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}" || exit 1

  utxos=$(bria_cmd list-utxos -w "${E2E_BRIA_WALLET}")
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

  external_wallet_send_to_address "${bitcoind_signer_address}" 1
  external_wallet_send_to_address "${bitcoind_signer_address}" 1

  bitcoind_address=$(external_wallet_new_address)
  retry 20 1 signer_unconfirmed_balance_is "2.00000000"
  signer_unconfirmed_balance_is "2.00000000" || exit 1

  bitcoin_signer_cli_send_all_utxos \
    2.1 \
    0.38 \
    ${bitcoind_address}

  retry 60 1 wallet_pending_outgoing_is 210000000 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 210000000 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_effective_settled_is_not 0 "${E2E_BRIA_WALLET}"
  wallet_effective_settled_is_not 0 "${E2E_BRIA_WALLET}" || exit 1

  bitcoin_cli -generate 2
  retry 60 1 wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}" || exit 1

  retry 60 1 wallet_effective_settled_matches_signer_balance "${E2E_BRIA_WALLET}"
  wallet_effective_settled_matches_signer_balance "${E2E_BRIA_WALLET}" || exit 1
}

@test "bitcoind_signer_sync: Can sweep all" {
  cache_wallet_balance "${E2E_BRIA_WALLET}"
  [[ $(cached_current_settled) != 0 ]] || exit 1

  bitcoind_address=$(external_wallet_new_address)
  bitcoin_signer_cli -named sendall recipients="[\"${bitcoind_address}\"]" fee_rate=1
  retry 60 1 wallet_current_settled_is_zero_and_pending_outgoing_is_not_zero "${E2E_BRIA_WALLET}"
  wallet_current_settled_is_zero_and_pending_outgoing_is_not_zero "${E2E_BRIA_WALLET}" || exit 1

  bitcoin_cli -generate 1
  retry 60 1 wallet_pending_outgoing_and_encumbered_fees_are_zero "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_and_encumbered_fees_are_zero "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_effective_settled_is 0 "${E2E_BRIA_WALLET}"
  wallet_effective_settled_is 0 "${E2E_BRIA_WALLET}" || exit 1
}

@test "bitcoind_signer_sync: Can spend only from unconfirmed" {
  bitcoind_signer_address=$(bitcoin_signer_cli getnewaddress)
  external_wallet_send_to_address "${bitcoind_signer_address}" 1
  retry 20 1 signer_unconfirmed_balance_is "1.00000000"
  signer_unconfirmed_balance_is "1.00000000" || exit 1

  bitcoind_address=$(external_wallet_new_address)
  bitcoin_signer_cli_send_all_utxos \
    0.6 \
    0.39 \
    ${bitcoind_address}

  retry 60 1 wallet_pending_outgoing_is 60000000 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 60000000 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_effective_settled_is 0 "${E2E_BRIA_WALLET}"
  wallet_effective_settled_is 0 "${E2E_BRIA_WALLET}" || exit 1

  bitcoin_cli -generate 2
  retry 60 1 wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}"
  wallet_pending_outgoing_is 0 "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_effective_settled_matches_current_settled "${E2E_BRIA_WALLET}"
  wallet_effective_settled_matches_current_settled "${E2E_BRIA_WALLET}" || exit 1
  retry 60 1 wallet_effective_settled_matches_signer_balance "${E2E_BRIA_WALLET}"
  wallet_effective_settled_matches_signer_balance "${E2E_BRIA_WALLET}" || exit 1
}
