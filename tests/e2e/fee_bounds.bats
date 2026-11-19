#!/usr/bin/env bats

load "helpers"

setup_file() {
  e2e_init_file_context
  e2e_create_default_wallet_pair
  E2E_QUEUE_FEE_BOUNDS="$(e2e_queue_name fee_bounds)"
  e2e_save_context
}

setup() {
  e2e_load_context
}

teardown_file() {
  # Later files expect the daemon on the regular config.
  stop_daemon
  start_daemon
}

# Regtest estimates are 1 sat/vB, so a 0.5 ceiling refuses them; blockstream points at a closed port so it can't offer a lower reading.
fee_bounds_config() {
  local config="${REPO_ROOT}/.bats-e2e/bria.fee_bounds.yml"
  awk '{ print } /^  fees:$/ {
    print "    blockstream:"
    print "      url: http://127.0.0.1:9"
    print "      number_of_retries: 0"
    print "    min_sat_per_vb: 0.1"
    print "    max_sat_per_vb: 0.5"
  }' "${REPO_ROOT}/tests/e2e/bria.${BRIA_CONFIG:-local}.yml" > "${config}"
  echo "${config}"
}

payout_batch_id() {
  bria_cmd list-payouts -w "${E2E_BRIA_WALLET}" | jq -r '.payouts[0].batchId'
}

payout_is_batched() {
  [[ "$(payout_batch_id)" != "null" ]]
}

@test "fee_bounds: Payout stays unbatched while the fee rate is above max_sat_per_vb" {
  bria_address=$(bria_cmd new-address -w "${E2E_BRIA_WALLET}" | jq -r '.address')
  external_wallet_send_to_address "${bria_address}" 1
  bitcoin_cli -generate 10
  retry 60 1 wallet_current_settled_is 100000000 "${E2E_BRIA_WALLET}"
  wallet_current_settled_is 100000000 "${E2E_BRIA_WALLET}" || exit 1

  stop_daemon
  start_daemon "$(fee_bounds_config)"

  bria_cmd create-payout-queue --name "${E2E_QUEUE_FEE_BOUNDS}" --interval-trigger 2
  bria_cmd submit-payout --wallet "${E2E_BRIA_WALLET}" --queue-name "${E2E_QUEUE_FEE_BOUNDS}" --destination bcrt1q208tuy5rd3kvy8xdpv6yrczg7f3mnlk3lql7ej --amount 50000000

  retry 30 1 grep -q 'payout queue deferred to its next tick' .e2e-logs
  grep 'payout queue deferred to its next tick' .e2e-logs | grep -q 'FeeRateAboveMaximum' || exit 1

  # Several more ticks: the queue keeps deferring rather than batching or dropping the payout.
  sleep 6
  [[ "$(payout_batch_id)" == "null" ]] || exit 1
  n_payouts=$(bria_cmd list-payouts -w "${E2E_BRIA_WALLET}" | jq '.payouts | length')
  [[ "${n_payouts}" == "1" ]] || exit 1
}

@test "fee_bounds: Deferred payout is batched once the fee rate is back in bounds" {
  stop_daemon
  start_daemon

  retry 60 1 payout_is_batched
  payout_is_batched || exit 1
}
