#!/usr/bin/env bash

setup_suite() {
  local repo_root
  repo_root=$(git rev-parse --show-toplevel)
  source "${repo_root}/tests/e2e/helpers.bash"
  bitcoind_base_init
  start_daemon

  if ! bria_cmd admin list-accounts >/dev/null 2>&1; then
    bria_cmd admin bootstrap
  fi

  if ! bria_cmd admin list-accounts | jq -e '.accounts[] | select(.name == "default")' >/dev/null 2>&1; then
    bria_cmd admin create-account -n default
  fi
}

teardown_suite() {
  local repo_root
  repo_root=$(git rev-parse --show-toplevel)
  source "${repo_root}/tests/e2e/helpers.bash"
  stop_daemon
}
