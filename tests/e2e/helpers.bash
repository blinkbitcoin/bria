REPO_ROOT=$(git rev-parse --show-toplevel)
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-${REPO_ROOT##*/}}"
SIGNER_ENCRYPTION_KEY="0000000000000000000000000000000000000000000000000000000000000000"
BRIA_HOME="${BRIA_HOME:-.bria}"
export PG_CON="${PG_CON:-${DATABASE_URL}}"
if [[ "${BRIA_CONFIG}" == "docker" ]]; then
  COMPOSE_FILE_ARG="-f docker-compose.yml"
fi
if [[ "${BITCOIND_SIGNER_ENDPOINT:-}" == *"/wallet/"* ]]; then
  BITCOIND_SIGNER_ENDPOINT_BASE="${BITCOIND_SIGNER_ENDPOINT%%/wallet/*}"
else
  BITCOIND_SIGNER_ENDPOINT_BASE="${BITCOIND_SIGNER_ENDPOINT:-https://localhost:18543}"
fi
BITCOIND_SIGNER_ENDPOINT="${BITCOIND_SIGNER_ENDPOINT_BASE}"
SATS_IN_ONE_BTC=100000000

bria_cmd() {
  bria_location=${REPO_ROOT}/target/debug/bria
  if [[ ! -z ${CARGO_TARGET_DIR} ]] ; then
    bria_location=${CARGO_TARGET_DIR}/debug/bria
  fi

  ${bria_location} $@
}

cache_wallet_balance() {
  local wallet_name="${1:-default}"
  balance=$(bria_cmd wallet-balance -w "${wallet_name}")
}

cached_pending_income() {
  echo ${balance} | jq -r '.utxoPendingIncoming'
}

cached_encumbered_fees() {
  echo ${balance} | jq -r '.feesEncumbered'
}

cached_current_settled() {
  echo ${balance} | jq -r '.utxoSettled'
}

cached_effective_settled() {
  echo ${balance} | jq -r '.effectiveSettled'
}

cached_pending_outgoing() {
  echo ${balance} | jq -r '.effectivePendingOutgoing'
}

cached_pending_fees() {
  echo ${balance} | jq -r '.feesPending'
}

cached_encumbered_outgoing() {
  echo ${balance} | jq -r '.effectiveEncumberedOutgoing'
}

bitcoin_cli() {
  docker exec "${COMPOSE_PROJECT_NAME}-bitcoind-1" bitcoin-cli $@
}

external_wallet_new_address() {
  bitcoin_cli -rpcwallet=default -regtest getnewaddress
}

external_wallet_send_to_address() {
  local address="$1"
  local amount="$2"
  bitcoin_cli -rpcwallet=default -regtest sendtoaddress "${address}" "${amount}"
}

bitcoin_signer_cli() {
  local wallet_arg=""
  if [[ -n "${E2E_BITCOIND_SIGNER_WALLET:-}" && "${1:-}" != -rpcwallet=* ]]; then
    wallet_arg="-rpcwallet=${E2E_BITCOIND_SIGNER_WALLET}"
  fi
  docker exec "${COMPOSE_PROJECT_NAME}-bitcoind-signer-1" bitcoin-cli ${wallet_arg} "$@"
}

bitcoin_signer_wallet_cli() {
  local wallet="$1"
  shift
  docker exec "${COMPOSE_PROJECT_NAME}-bitcoind-signer-1" bitcoin-cli "-rpcwallet=${wallet}" "$@"
}

convert_btc_to_sats() {
  echo "$1 * $SATS_IN_ONE_BTC / 1" | bc
}

bitcoin_signer_cli_send_all_utxos () {
  amount=$1
  change=$2
  send_address=$3

  rawtx_utxos=$(bitcoin_signer_cli listunspent 0 | jq -c '[.[] | {txid: .txid, vout: .vout}]')

  change_address=$(bitcoin_signer_cli getrawchangeaddress "bech32")
  rawtx_addresses="[{\"${send_address}\":$amount},{\"${change_address}\":$change}]"

  unsigned_tx=$(bitcoin_signer_cli createrawtransaction $rawtx_utxos $rawtx_addresses)
  signed_tx=$(bitcoin_signer_cli signrawtransactionwithwallet $unsigned_tx | jq -r '.hex')
  bitcoin_signer_cli sendrawtransaction $signed_tx
}


lnd_cli() {
  docker exec "${COMPOSE_PROJECT_NAME}-lnd-1" lncli -n regtest $@
}

lnd_unconfirmed_balance_is() {
  local expected="$1"
  [[ "$(lnd_cli walletbalance | jq -r '.unconfirmed_balance')" == "${expected}" ]]
}

bitcoin_mempool_has_tx() {
  local txid="$1"
  bitcoin_cli -rpcwallet=default -regtest getmempoolentry "${txid}" >/dev/null 2>&1
}

restart_bitcoin_stack() {
  docker compose ${COMPOSE_FILE_ARG} rm -sfv bitcoind bitcoind-signer lnd fulcrum mempool || true
  # Running this twice has sometimes bitcoind is dangling in CI
  docker compose ${COMPOSE_FILE_ARG} rm -sfv bitcoind bitcoind-signer lnd fulcrum mempool || true
  docker compose ${COMPOSE_FILE_ARG} up -d bitcoind bitcoind-signer lnd fulcrum mempool
  retry 10 1 lnd_cli getinfo
}

bitcoind_init() {
  local wallet="${1:-default}"

  bitcoin_cli createwallet "default" || true
  bitcoin_cli loadwallet "default" || true
  bitcoin_cli generatetoaddress 200 "$(bitcoin_cli getnewaddress)"

  if [[ "${wallet}" == "default" ]]; then
    bitcoin_signer_cli createwallet "default" || true
    bitcoin_signer_cli loadwallet "default" || true
    bitcoin_signer_cli -rpcwallet=default importdescriptors "$(cat ${REPO_ROOT}/tests/e2e/bitcoind_signer_descriptors.json)"
  elif [[ "${wallet}" == "multisig" ]]; then
    bitcoin_signer_cli createwallet "multisig" || true
    bitcoin_signer_cli loadwallet "multisig" || true
    bitcoin_signer_cli -rpcwallet=multisig importdescriptors "$(cat ${REPO_ROOT}/tests/e2e/bitcoind_multisig_signer_descriptors.json)"
    bitcoin_signer_cli createwallet "multisig2" || true
    bitcoin_signer_cli -rpcwallet=multisig2 importdescriptors "$(cat ${REPO_ROOT}/tests/e2e/bitcoind_multisig2_signer_descriptors.json)"
  fi
}

bitcoind_base_init() {
  bitcoin_cli createwallet "default" || true
  bitcoin_cli generatetoaddress 200 "$(bitcoin_cli getnewaddress)"
}

e2e_random_suffix() {
  printf '%s_%s' "$(date +%s)" "${RANDOM}${RANDOM}"
}

e2e_init_file_context() {
  local file_name="${1:-${BATS_TEST_FILENAME##*/}}"
  file_name="${file_name%.bats}"
  file_name="${file_name//[^a-zA-Z0-9_]/_}"
  E2E_FILE_SCOPE="${file_name}_$(e2e_random_suffix)"
}

e2e_scoped_name() {
  local prefix="$1"
  echo "${prefix}_${E2E_FILE_SCOPE}"
}

e2e_queue_name() {
  local name="$1"
  e2e_scoped_name "queue_${name}"
}

e2e_bitcoind_signer_endpoint() {
  local wallet="${1:-${E2E_BITCOIND_SIGNER_WALLET}}"
  echo "${BITCOIND_SIGNER_ENDPOINT_BASE}/wallet/${wallet}"
}

e2e_extract_signer_fingerprint() {
  local descriptor="$1"
  sed -E 's/.*\[([0-9a-fA-F]+)\/.*$/\1/' <<< "${descriptor}"
}

e2e_extract_descriptor_xpub() {
  local descriptor="$1"
  sed -nE "s/.*(tpub[1-9A-HJ-NP-Za-km-z]+).*/\1/p" <<< "${descriptor}"
}

e2e_create_default_wallet_pair() {
  E2E_BITCOIND_SIGNER_WALLET="$(e2e_scoped_name signer)"
  E2E_BRIA_WALLET="$(e2e_scoped_name wallet)"

  docker exec "${COMPOSE_PROJECT_NAME}-bitcoind-signer-1" bitcoin-cli createwallet "${E2E_BITCOIND_SIGNER_WALLET}" >/dev/null

  local private_descriptors
  private_descriptors=$(bitcoin_signer_wallet_cli "${E2E_BITCOIND_SIGNER_WALLET}" listdescriptors false)
  E2E_SIGNER_EXTERNAL_DESCRIPTOR=$(jq -r '[.descriptors[] | select((.active // false) == true and (.internal // false) == false and (.desc | startswith("wpkh(")))][0].desc // [.descriptors[] | select((.active // false) == true and (.internal // false) == false)][0].desc' <<< "${private_descriptors}")
  E2E_SIGNER_INTERNAL_DESCRIPTOR=$(jq -r '[.descriptors[] | select((.active // false) == true and (.internal // false) == true and (.desc | startswith("wpkh(")))][0].desc // [.descriptors[] | select((.active // false) == true and (.internal // false) == true)][0].desc' <<< "${private_descriptors}")
  E2E_SIGNER_XPUB_REF=$(e2e_extract_signer_fingerprint "${E2E_SIGNER_EXTERNAL_DESCRIPTOR}")

  bria_cmd create-wallet -n "${E2E_BRIA_WALLET}" descriptors \
    -d "${E2E_SIGNER_EXTERNAL_DESCRIPTOR}" \
    -c "${E2E_SIGNER_INTERNAL_DESCRIPTOR}"

  BITCOIND_SIGNER_ENDPOINT="$(e2e_bitcoind_signer_endpoint "${E2E_BITCOIND_SIGNER_WALLET}")"

  local signer_xpub
  signer_xpub=$(e2e_extract_descriptor_xpub "${E2E_SIGNER_EXTERNAL_DESCRIPTOR}")
  E2E_SIGNER_XPUB_REF=$(bria_cmd list-xpubs | jq -r --arg xpub "${signer_xpub}" --arg fp "${E2E_SIGNER_XPUB_REF}" '([.xpubs[] | select(.xpub == $xpub) | .id][0]) // ([.xpubs[] | select(.id == $fp or .name == $fp or (.id | startswith($fp))) | .id][0]) // $fp')
}

e2e_ensure_default_signer_wallet_loaded() {
  if [[ -z "${E2E_BITCOIND_SIGNER_WALLET:-}" ]]; then
    echo "[e2e] signer wallet var is empty, skipping load guard" >&3
    return
  fi

  echo "[e2e] ensure signer wallet loaded: ${E2E_BITCOIND_SIGNER_WALLET}" >&3
  echo "[e2e] signer wallets before: $(bitcoin_signer_cli listwallets 2>/dev/null || true)" >&3

  if bitcoin_signer_wallet_cli "${E2E_BITCOIND_SIGNER_WALLET}" getwalletinfo >/dev/null 2>&1; then
    BITCOIND_SIGNER_ENDPOINT="$(e2e_bitcoind_signer_endpoint "${E2E_BITCOIND_SIGNER_WALLET}")"
    echo "[e2e] signer wallet already loaded, base=${BITCOIND_SIGNER_ENDPOINT_BASE}, endpoint=${BITCOIND_SIGNER_ENDPOINT}" >&3
    return
  fi

  echo "[e2e] loading signer wallet ${E2E_BITCOIND_SIGNER_WALLET}" >&3
  bitcoin_signer_cli loadwallet "${E2E_BITCOIND_SIGNER_WALLET}" >/dev/null 2>&1 || true

  if ! bitcoin_signer_wallet_cli "${E2E_BITCOIND_SIGNER_WALLET}" getwalletinfo >/dev/null 2>&1; then
    echo "[e2e] signer wallet not available after load, creating/importing descriptors" >&3
    docker exec "${COMPOSE_PROJECT_NAME}-bitcoind-signer-1" bitcoin-cli createwallet "${E2E_BITCOIND_SIGNER_WALLET}" >/dev/null 2>&1 || true
    if [[ -n "${E2E_SIGNER_EXTERNAL_DESCRIPTOR:-}" && -n "${E2E_SIGNER_INTERNAL_DESCRIPTOR:-}" ]]; then
      local descriptor_payload
      descriptor_payload=$(jq -cn \
        --arg ext "${E2E_SIGNER_EXTERNAL_DESCRIPTOR}" \
        --arg int "${E2E_SIGNER_INTERNAL_DESCRIPTOR}" \
        '[
          {"desc": $ext, "active": true, "timestamp": 0},
          {"desc": $int, "active": true, "internal": true, "timestamp": 0}
        ]')
      bitcoin_signer_wallet_cli "${E2E_BITCOIND_SIGNER_WALLET}" importdescriptors "${descriptor_payload}" >/dev/null 2>&1 || true
    fi
  fi

  BITCOIND_SIGNER_ENDPOINT="$(e2e_bitcoind_signer_endpoint "${E2E_BITCOIND_SIGNER_WALLET}")"
  echo "[e2e] signer wallets after: $(bitcoin_signer_cli listwallets 2>/dev/null || true)" >&3
  echo "[e2e] signer endpoint after ensure: ${BITCOIND_SIGNER_ENDPOINT}" >&3
}

e2e_create_multisig_wallet_set() {
  E2E_BITCOIND_SIGNER_WALLET="$(e2e_scoped_name signer_multisig)"
  E2E_BITCOIND_SIGNER_WALLET_2="$(e2e_scoped_name signer_multisig2)"
  E2E_BRIA_WALLET="$(e2e_scoped_name wallet_multisig)"
  E2E_MULTISIG_KEY_1="$(e2e_scoped_name key1)"
  E2E_MULTISIG_KEY_2="$(e2e_scoped_name key2)"

  docker exec "${COMPOSE_PROJECT_NAME}-bitcoind-signer-1" bitcoin-cli createwallet "${E2E_BITCOIND_SIGNER_WALLET}" >/dev/null
  bitcoin_signer_wallet_cli "${E2E_BITCOIND_SIGNER_WALLET}" importdescriptors "$(cat "${REPO_ROOT}/tests/e2e/bitcoind_multisig_signer_descriptors.json")"

  docker exec "${COMPOSE_PROJECT_NAME}-bitcoind-signer-1" bitcoin-cli createwallet "${E2E_BITCOIND_SIGNER_WALLET_2}" >/dev/null
  bitcoin_signer_wallet_cli "${E2E_BITCOIND_SIGNER_WALLET_2}" importdescriptors "$(cat "${REPO_ROOT}/tests/e2e/bitcoind_multisig2_signer_descriptors.json")"

  local key1="tpubDEaDfeS1EXpqLVASNCW7qAHW1TFPBpk2Z39gUXjFnsfctomZ7N8iDpy6RuGwqdXAAZ5sr5kQZrxyuEn15tqPJjM4mcPSuXzV27AWRD3p9Q4"
  local key2="tpubDEPCxBfMFRNdfJaUeoTmepLJ6ZQmeTiU1Sko2sdx1R3tmPpZemRUjdAHqtmLfaVrBg1NBx2Yx3cVrsZ2FTyBuhiH9mPSL5ozkaTh1iZUTZx"

  bria_cmd import-xpub -x "${key1}" -n "${E2E_MULTISIG_KEY_1}" -d m/48h/1h/0h/2h
  bria_cmd import-xpub -x "${key2}" -n "${E2E_MULTISIG_KEY_2}" -d m/48h/1h/0h/2h
  bria_cmd create-wallet -n "${E2E_BRIA_WALLET}" sorted-multisig -x "${E2E_MULTISIG_KEY_1}" "${E2E_MULTISIG_KEY_2}" -t 2
}

e2e_create_lnd_wallet() {
  E2E_BRIA_WALLET="$(e2e_scoped_name wallet_lnd)"
  bria_cmd create-wallet -n "${E2E_BRIA_WALLET}" descriptors \
    -d "wpkh([6f2fa1b2/84'/0'/0']tpubDD4vFnWuTMEcZiaaZPgvzeGyMzWe6qHW8gALk5Md9kutDvtdDjYFwzauEFFRHgov8pAwup5jX88j5YFyiACsPf3pqn5hBjvuTLRAseaJ6b4/0/*)#wlmk9vyk" \
    -c "tr([6f2fa1b2/86'/0'/0']tpubDD6sGNgWVAeKaMGF5XkfBhMAuSqjoiqUoSM7Dmf11auxu41PDg1AL4LDwTkuVEMUS2zY51zPESy1xr26cLj7BZHfwZQHd4Xf1Ym5WbvAMru/1/*)#ggr04sk2"
}

e2e_context_file() {
  local test_file="${BATS_TEST_FILENAME##*/}"
  echo "${REPO_ROOT}/.bats-e2e/${test_file}.env"
}

e2e_save_context() {
  local context_file
  context_file="$(e2e_context_file)"
  mkdir -p "${REPO_ROOT}/.bats-e2e"
  : > "${context_file}"

  local var_name
  for var_name in $(compgen -A variable E2E_); do
    printf 'export %s=%q\n' "${var_name}" "${!var_name}" >> "${context_file}"
  done
}

e2e_load_context() {
  local context_file
  context_file="$(e2e_context_file)"
  source "${context_file}"

  e2e_ensure_default_signer_wallet_loaded
}

start_daemon() {
  SIGNER_ENCRYPTION_KEY="${SIGNER_ENCRYPTION_KEY}" background bria_cmd daemon --config ./tests/e2e/bria.${BRIA_CONFIG:-local}.yml run > .e2e-logs
  for i in {1..20}
  do
    if head .e2e-logs | grep -q 'Starting main server on port'; then
      break
    else
      sleep 1
    fi
  done
}

stop_daemon() {
  if [[ -f ${BRIA_HOME}/daemon-pid ]]; then
    kill -9 $(cat ${BRIA_HOME}/daemon-pid) || true
  fi
}

# Run the given command in the background. Useful for starting a
# node and then moving on with commands that exercise it for the
# test.
#
# Ensures that BATS' handling of file handles is taken into account;
# see
# https://github.com/bats-core/bats-core#printing-to-the-terminal
# https://github.com/sstephenson/bats/issues/80#issuecomment-174101686
# for details.
background() {
  "$@" 3>- &
  echo $!
}

# Taken from https://github.com/docker/swarm/blob/master/test/integration/helpers.bash
# Retry a command $1 times until it succeeds. Wait $2 seconds between retries.
retry() {
  local attempts=$1
  shift
  local delay=$1
  shift
  local i
  local attempt_status

  for ((i = 0; i < attempts; i++)); do
    if [[ "${BATS_TEST_DIRNAME}" = "" ]]; then
      "$@"
      attempt_status=$?
    else
      run "$@"
      attempt_status=$status
    fi

    if [[ "$attempt_status" -eq 0 ]]; then
      return 0
    fi

    sleep "$delay"
  done

  echo "Command \"$*\" failed $attempts times. Output: $output"
  false
}

wallet_pending_outgoing_is() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_pending_outgoing)" == "${expected}" ]]
}

wallet_pending_income_is() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_pending_income)" == "${expected}" ]]
}

wallet_pending_income_is_not() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_pending_income)" != "${expected}" ]]
}

wallet_current_settled_is() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_current_settled)" == "${expected}" ]]
}

wallet_current_settled_ge() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ $(cached_current_settled) -ge ${expected} ]]
}

wallet_pending_outgoing_is_not() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_pending_outgoing)" != "${expected}" ]]
}

wallet_encumbered_outgoing_is() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_encumbered_outgoing)" == "${expected}" ]]
}

wallet_current_settled_is_not() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_current_settled)" != "${expected}" ]]
}

wallet_current_settled_or_pending_outgoing_is_not_zero() {
  local wallet_name="${1:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_current_settled)" != "0" || "$(cached_pending_outgoing)" != "0" ]]
}

wallet_encumbered_fees_is() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_encumbered_fees)" == "${expected}" ]]
}

wallet_effective_settled_is() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_effective_settled)" == "${expected}" ]]
}

wallet_encumbered_outgoing_is_and_effective_settled_ge() {
  local encumbered_expected="$1"
  local effective_settled_min="$2"
  local wallet_name="${3:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_encumbered_outgoing)" == "${encumbered_expected}" && $(cached_effective_settled) -ge ${effective_settled_min} ]]
}

wallet_encumbered_outgoing_is_and_effective_settled_is() {
  local encumbered_expected="$1"
  local effective_settled_expected="$2"
  local wallet_name="${3:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_encumbered_outgoing)" == "${encumbered_expected}" && "$(cached_effective_settled)" == "${effective_settled_expected}" ]]
}

wallet_effective_settled_matches_current_settled() {
  local wallet_name="${1:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_effective_settled)" == "$(cached_current_settled)" ]]
}

wallet_current_settled_is_zero_and_pending_outgoing_is_not_zero() {
  local wallet_name="${1:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_current_settled)" == "0" && "$(cached_pending_outgoing)" != "0" ]]
}

wallet_pending_outgoing_and_encumbered_fees_are_zero() {
  local wallet_name="${1:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_pending_outgoing)" == "0" && "$(cached_encumbered_fees)" == "0" ]]
}

wallet_encumbered_outgoing_is_zero() {
  local wallet_name="${1:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_encumbered_outgoing)" == "0" ]]
}

wallet_effective_settled_is_not() {
  local expected="$1"
  local wallet_name="${2:-default}"

  cache_wallet_balance "${wallet_name}"
  [[ "$(cached_effective_settled)" != "${expected}" ]]
}

signer_unconfirmed_balance_is() {
  local expected="$1"
  [[ "$(bitcoin_signer_cli getunconfirmedbalance)" == "${expected}" ]]
}

wallet_effective_settled_matches_signer_balance() {
  local wallet_name="${1:-default}"
  local bitcoind_signer_balance_in_btc
  local bitcoind_signer_balance

  cache_wallet_balance "${wallet_name}"
  bitcoind_signer_balance_in_btc=$(bitcoin_signer_cli getbalance)
  bitcoind_signer_balance=$(convert_btc_to_sats "${bitcoind_signer_balance_in_btc}")

  [[ "$(cached_effective_settled)" == "${bitcoind_signer_balance}" ]]
}

wallet_effective_settled_matches_lnd_balance() {
  local wallet_name="${1:-default}"
  local lnd_balance

  cache_wallet_balance "${wallet_name}"
  lnd_balance=$(lnd_cli walletbalance | jq -r '.total_balance')

  [[ "$(cached_effective_settled)" == "${lnd_balance}" ]]
}
