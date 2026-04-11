#!/usr/bin/env bats

load "helpers"

setup_file() {
  restart_bitcoin_stack
  reset_pg
  bitcoind_init
  start_daemon
  bria_lnd_init
}

teardown_file() {
  stop_daemon
}

@test "lnd_sync: Generates the same address" {
  lnd_address=$(lnd_cli newaddress p2wkh | jq -r '.address')
  bria_address=$(bria_cmd new-address -w default | jq -r '.address')

  [ "$lnd_address" = "$bria_address" ]

  n_addresses=$(bria_cmd list-addresses -w default | jq -r '.addresses | length')
  [ "$n_addresses" = "1" ] || exit 1
}

@test "lnd_sync: Detects incoming transactions" {
  lnd_address=$(lnd_cli newaddress p2wkh | jq -r '.address')
  if [ -z "$lnd_address" ]; then
    echo "Failed to get a new address"
    exit 1
  fi

  bitcoin_cli -regtest sendtoaddress ${lnd_address} 1

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

  [[ "${n_utxos}" == "1" && "${utxo_block_height}" == "201" ]]
}

@test "lnd_sync: Detects outgoing transactions" {
  bitcoind_address=$(bitcoin_cli -regtest getnewaddress)
  lnd_cli sendcoins --addr=${bitcoind_address} --amt=50000000
  retry 60 1 wallet_pending_outgoing_is 50000000
  wallet_pending_outgoing_is 50000000 || exit 1
  retry 60 1 wallet_current_settled_is 0
  wallet_current_settled_is 0 || exit 1

  utxos=$(bria_cmd list-utxos -w default)
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  change=$(jq -r '.keychains[0].utxos[0].changeOutput' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${change}" == "true" ]]

  bitcoin_cli -generate 1

  retry 60 1 wallet_current_settled_is_not 0
  wallet_current_settled_is_not 0 || exit 1
  retry 60 1 wallet_pending_outgoing_is 0
  wallet_pending_outgoing_is 0 || exit 1

  utxos=$(bria_cmd list-utxos -w default)
  n_utxos=$(jq '.keychains[0].utxos | length' <<< "${utxos}")
  utxo_block_height=$(jq -r '.keychains[0].utxos[0].blockHeight' <<< "${utxos}")

  [[ "${n_utxos}" == "1" && "${utxo_block_height}" == "203" ]]
}

@test "lnd_sync: Can handle spend from mix of unconfirmed UTXOs" {
  lnd_address=$(lnd_cli newaddress p2wkh | jq -r '.address')
  if [ -z "$lnd_address" ]; then
    echo "Failed to get a new address"
    exit 1
  fi

  bitcoin_cli -regtest sendtoaddress ${lnd_address} 1
  bitcoin_cli -regtest sendtoaddress ${lnd_address} 1

  bitcoind_address=$(bitcoin_cli -regtest getnewaddress)
  lnd_cli sendcoins --addr=${bitcoind_address} --amt=210000000 --min_confs 0

  retry 60 1 wallet_pending_outgoing_is 210000000
  wallet_pending_outgoing_is 210000000 || exit 1
  retry 60 1 wallet_effective_settled_is_not 0
  wallet_effective_settled_is_not 0 || exit 1

  bitcoin_cli -generate 2
  retry 60 1 wallet_pending_outgoing_is 0
  wallet_pending_outgoing_is 0 || exit 1

  retry 60 1 wallet_effective_settled_matches_lnd_balance
  wallet_effective_settled_matches_lnd_balance || exit 1
}

@test "lnd_sync: Can sweep all" {
  bitcoind_address=$(bitcoin_cli -regtest getnewaddress)
  lnd_cli sendcoins --addr=${bitcoind_address} --sweepall
  bitcoin_cli -generate 1

  retry 60 1 wallet_encumbered_fees_is 0
  wallet_encumbered_fees_is 0 || exit 1
  retry 60 1 wallet_effective_settled_is 0
  wallet_effective_settled_is 0 || exit 1
}

@test "lnd_sync: Can spend only from unconfirmed" {
  lnd_address=$(lnd_cli newaddress p2wkh | jq -r '.address')
  bitcoin_cli -regtest sendtoaddress ${lnd_address} 1
  bitcoind_address=$(bitcoin_cli -regtest getnewaddress)
  lnd_cli sendcoins --addr=${bitcoind_address} --amt=60000000 --min_confs 0

  retry 60 1 wallet_pending_outgoing_is 60000000
  wallet_pending_outgoing_is 60000000 || exit 1
  retry 60 1 wallet_effective_settled_is 0
  wallet_effective_settled_is 0 || exit 1

  bitcoin_cli -generate 2
  retry 60 1 wallet_pending_outgoing_is 0
  wallet_pending_outgoing_is 0 || exit 1
  retry 60 1 wallet_effective_settled_matches_current_settled
  wallet_effective_settled_matches_current_settled || exit 1
  retry 60 1 wallet_effective_settled_matches_lnd_balance
  wallet_effective_settled_matches_lnd_balance || exit 1
}
