#!/usr/bin/env bats

load "helpers"

setup_file() {
  e2e_init_file_context
  e2e_create_default_wallet_pair
  E2E_QUEUE_HIGH="$(e2e_queue_name high)"
  E2E_OUTBOX_BASE=$(docker exec "${COMPOSE_PROJECT_NAME}-postgres-1" psql "${PG_CON}" -t -A -c "SELECT COALESCE(MAX(sequence), -1) FROM bria_outbox_events" | tr -d '[:space:]')
  e2e_save_context
}

setup() {
  e2e_load_context
}

@test "outbox: Emits utxo_dropped event" {
  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')
  external_wallet_send_to_address "${bria_address}" 1
  retry 60 1 wallet_pending_income_is 100000000 "${E2E_BRIA_WALLET}"
  wallet_pending_income_is 100000000 "${E2E_BRIA_WALLET}" || exit 1
  event=$(bria_cmd watch-events -a "${E2E_OUTBOX_BASE}" -o | jq -r '.payload.utxoDetected')
  [ "$event" != "null" ] || exit 1

  restart_bitcoin_stack
  bitcoind_init

  event=$(bria_cmd watch-events -a $((E2E_OUTBOX_BASE + 1)) -o | jq -r '.payload.utxoDropped')
  [ "$event" != "null" ] || exit 1

  retry 60 1 wallet_pending_income_is 0 "${E2E_BRIA_WALLET}"
  wallet_pending_income_is 0 "${E2E_BRIA_WALLET}" || exit 1
}

@test "outbox: Adds address augmentation to events" {
  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" -m '{"hello":"world"}' | jq -r '.address')
  external_wallet_send_to_address "${bria_address}" 1
  event=$(bria_cmd watch-events -a $((E2E_OUTBOX_BASE + 2)) -o | jq -r '.augmentation')
  [ "$event" = "null" ] || exit 1
  meta=$(bria_cmd watch-events -a $((E2E_OUTBOX_BASE + 2)) -o --augment | jq -r '.augmentation.addressInfo.metadata.hello')
  [ "$meta" = "world" ] || exit 1
  bria_cmd update-address -a "${bria_address}" -m '{"other":"world"}'
  meta=$(bria_cmd watch-events -a $((E2E_OUTBOX_BASE + 2)) -o --augment | jq -r '.augmentation.addressInfo.metadata.other')
  [ "$meta" = "world" ] || exit 1
}

@test "outbox: Adds payout augmentation info to events" {
  bria_cmd create-payout-queue --name "${E2E_QUEUE_HIGH}" --interval-trigger 5
  bria_cmd submit-payout --wallet "${E2E_BRIA_WALLET}" --queue-name "${E2E_QUEUE_HIGH}" --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej --amount 75000000 -e "external"
  external_id=$(bria_cmd watch-events -a $((E2E_OUTBOX_BASE + 3)) -o --augment | jq -r '.augmentation.payoutInfo.externalId')
  [ "$external_id" = "external" ] || exit 1
}
