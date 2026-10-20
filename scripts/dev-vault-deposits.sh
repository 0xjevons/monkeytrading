#!/usr/bin/env bash
# Dev scenario for vault deposits, on a local Anvil chain with a node on config/anvil.toml:
#   1. start Anvil and deploy a mock USDC and the vault;
#   2. run a node with custody.provider = "evm" against them;
#   3. deposit as a trader and as the house, restart the node mid-way (once with its cursor lost),
#      and deposit again;
#   4. reorg a deposit away before it is final, then include the same transaction again.
# Passes when every deposit is credited once, only after the configured confirmations, house NAV
# equals the house deposits, and the node's log replays.
#
# Needs anvil, forge, cast, curl, and jq, and the contract dependencies
# (`forge soldeer install` in contracts/). ANVIL_PORT and API_PORT override 8545 and 8080.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
anvil_port="${ANVIL_PORT:-8545}"
api_port="${API_PORT:-8080}"
rpc="http://127.0.0.1:$anvil_port"
api="http://127.0.0.1:$api_port"
# Anvil's default test mnemonic: account 0 deploys, 1 trades, 2 funds the house.
mnemonic="test test test test test test test test test test test junk"
key() { cast wallet private-key --mnemonic "$mnemonic" --mnemonic-index "$1"; }
deployer_key="$(key 0)"
trader_key="$(key 1)"
house_key="$(key 2)"
trader="$(cast wallet address --private-key "$trader_key" | tr '[:upper:]' '[:lower:]')"
usdc_units=1000000
anvil_pid=""
node_pid=""

cleanup() {
  [[ -n "$node_pid" ]] && kill "$node_pid" 2>/dev/null || true
  [[ -n "$anvil_pid" ]] && kill "$anvil_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

step() { echo "==> $*"; }
fail() {
  echo "FAIL: $*" >&2
  [[ -f "$work/node.log" ]] && tail -n 40 "$work/node.log" >&2
  exit 1
}

# Polls `$1` (a shell function name) until it prints `$2`, for up to 20 seconds.
wait_for() {
  local got=""
  for _ in $(seq 1 100); do
    got="$("$1")"
    [[ "$got" == "$2" ]] && return 0
    sleep 0.2
  done
  fail "$1: expected $2, got $got"
}

# Waits long enough for several watcher polls, then checks `$1` still prints `$2`.
stays() {
  sleep 1.5
  local got
  got="$("$1")"
  [[ "$got" == "$2" ]] || fail "$1: expected $2 to hold, got $got"
}

trader_balance() { curl -s "$api/v1/accounts/$trader" | jq -r '.balance // "none"'; }
house_nav() { curl -s "$api/v1/house" | jq -r '.nav'; }
rescanned_deposits() { jq -r '.next_deposit_id' "$work/custody-cursor.json" 2>/dev/null || echo none; }
mine() { cast rpc anvil_mine "$(printf '0x%x' "$1")" --rpc-url "$rpc" >/dev/null; }
send() { cast send "$@" --rpc-url "$rpc" >/dev/null; }

start_node() {
  curl -s "$api/v1/health" >/dev/null 2>&1 && fail "port $api_port already serves an API"
  "$engine" run --config "$work/anvil.toml" >>"$work/node.log" 2>&1 &
  node_pid=$!
  for _ in $(seq 1 100); do
    kill -0 "$node_pid" 2>/dev/null || fail "node exited at startup"
    curl -sf "$api/v1/health" >/dev/null && return 0
    sleep 0.2
  done
  fail "node did not come up"
}

stop_node() {
  kill -TERM "$node_pid"
  wait "$node_pid" || fail "node did not shut down cleanly"
  node_pid=""
}

# Built first, so a slow build cannot leave the chain's head stale by the time the node starts.
step "Build the node"
cargo build -q --locked -p engine-node --manifest-path "$root/Cargo.toml"
# Wherever Cargo put it: `CARGO_TARGET_DIR` or `build.target-dir` may move the target directory.
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

step "Start Anvil; with one slot per epoch its safe block trails the head by one"
cast block-number --rpc-url "$rpc" >/dev/null 2>&1 && fail "port $anvil_port already serves an RPC"
anvil --port "$anvil_port" --slots-in-an-epoch 1 --silent &
anvil_pid=$!
anvil_up=""
for _ in $(seq 1 50); do
  kill -0 "$anvil_pid" 2>/dev/null || fail "anvil exited at startup"
  cast block-number --rpc-url "$rpc" >/dev/null 2>&1 && anvil_up=yes && break
  sleep 0.2
done
[[ -n "$anvil_up" ]] || fail "anvil did not come up"

step "Deploy the mock USDC and the vault"
cd "$root/contracts"
# `--constructor-args` takes every argument after it, so the contract and its arguments go last.
create() { forge create --rpc-url "$rpc" --private-key "$deployer_key" --broadcast --json "$@" | jq -r '.deployedTo'; }
usdc="$(create test/mocks/MockUSDC.sol:MockUSDC)"
vault="$(create src/Vault.sol:Vault --constructor-args "$usdc" "$usdc_units")"
cd "$root"
grep -qi "vault = \"$vault\"" config/anvil.toml || fail "vault deployed at $vault, not the address config/anvil.toml names"
grep -qi "usdc = \"$usdc\"" config/anvil.toml || fail "USDC deployed at $usdc, not the address config/anvil.toml names"
for funder_key in "$trader_key" "$house_key"; do
  funder="$(cast wallet address --private-key "$funder_key")"
  send "$usdc" "mint(address,uint256)" "$funder" $((10000000 * usdc_units)) --private-key "$deployer_key"
  send "$usdc" "approve(address,uint256)" "$vault" $((10000000 * usdc_units)) --private-key "$funder_key"
done

step "Start the node with EVM custody"
sed -e "s#data/anvil#$work#g" -e "s#127.0.0.1:8545#127.0.0.1:$anvil_port#" -e "s#127.0.0.1:8080#127.0.0.1:$api_port#" \
  config/anvil.toml >"$work/anvil.toml"
start_node
[[ "$(house_nav)" == "0" ]] || fail "genesis credited house capital under EVM custody"
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$api/v1/dev/deposit" \
  -H 'content-type: application/json' -d "{\"account\":\"$trader\",\"amount\":\"1\"}")"
[[ "$code" == "404" ]] || fail "the mock-only deposit endpoint answered $code under EVM custody"
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$api/v1/withdrawals" \
  -H 'content-type: application/json' -d "{\"account\":\"$trader\",\"amount\":\"1\"}")"
[[ "$code" == "501" ]] || fail "a withdrawal answered $code under EVM custody, which cannot pay it out yet"

step "A trader deposit credits only once three blocks have passed"
send "$vault" "deposit(uint256)" $((1000 * usdc_units)) --private-key "$trader_key"
stays trader_balance none
mine 2
stays trader_balance none
mine 1
wait_for trader_balance 1000

step "A house deposit adds house capital"
send "$vault" "depositToHouse(uint256)" $((5000000 * usdc_units)) --private-key "$house_key"
mine 3
wait_for house_nav 5000000

step "A deposit made while the node is down is credited after the restart"
stop_node
send "$vault" "deposit(uint256)" $((250 * usdc_units)) --private-key "$trader_key"
mine 3
start_node
wait_for trader_balance 1250

step "With its cursor lost, the node reads every deposit again and credits none twice"
stop_node
rm "$work/custody-cursor.json"
start_node
wait_for rescanned_deposits 3
stays trader_balance 1250
[[ "$(house_nav)" == "5000000" ]] || fail "house NAV changed on the rescan"

step "A deposit reorged away before it is final is never credited"
snapshot="$(cast rpc evm_snapshot --rpc-url "$rpc" | tr -d '"')"
nonce="$(cast nonce "$trader" --rpc-url "$rpc")"
raw="$(cast mktx "$vault" "deposit(uint256)" $((77 * usdc_units)) --private-key "$trader_key" --nonce "$nonce" \
  --gas-limit 200000 --rpc-url "$rpc")"
cast publish "$raw" --rpc-url "$rpc" >/dev/null
mine 1
stays trader_balance 1250
cast rpc evm_revert "$snapshot" --rpc-url "$rpc" >/dev/null
mine 5
stays trader_balance 1250

step "The same transaction included again is credited once"
cast publish "$raw" --rpc-url "$rpc" >/dev/null
mine 3
wait_for trader_balance 1327
mine 3
stays trader_balance 1327

step "The log holds one command per deposit and replays"
stop_node
deposits="$(jq -c 'select(.entry == "command" and (.cmd.type == "deposit" or .cmd.type == "house_capital"))' \
  "$work"/log/[0-9]*.jsonl | wc -l | tr -d ' ')"
[[ "$deposits" == "4" ]] || fail "expected 4 deposit commands in the log, found $deposits"
"$engine" replay --log "$work/log" | grep -q '^verified' || fail "the log does not replay"
# Restarts, the rescan, and the reorg are all expected: none of them may raise an alert.
if grep -q 'alert=' "$work/node.log"; then fail "unexpected alert: $(grep -m 1 'alert=' "$work/node.log")"; fi

echo "PASS: every deposit credited once after finality; house NAV equals house deposits"
