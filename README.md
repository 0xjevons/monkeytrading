# monkeytrading

monkeytrading is an open-source engine for trading contracts for difference on crypto-assets,
currencies, stock indices, commodities, and shares. As with a conventional CFD broker, the house
takes the other side of every trade. Unlike one, its rules are public code, it runs inside a
confidential-computing enclave, and it publishes every command it processes, so anyone can replay
the record and confirm that the state it reports is the state its rules produce. Ownership of the
house, in turn, is issued to the people who trade with it.

> **Pre-launch.** The engine runs end to end on a single machine, with a mock enclave, mock
> custody, and simulated prices, or live crypto prices from three venues; vault deposits also run
> against a local chain. On-chain withdrawals, hardware attestation, live prices for other asset
> classes, and the house equity token are under
> construction. Nothing here holds real funds.

## How it works

Every order trades against the house at a reference price: the median of recent quotes from
several independent venues. A market order fills immediately at that price; a limit or stop order
rests until the price reaches it. If too few venues have quoted recently, or their quotes disagree
by more than a set margin, the market has no price and nothing fills, so there is never a stale
price to pick off. A quote shown to a client is a guide rather than a promise: a market order fills
at the live price, and is declined only if that price is worse than the quote by more than the
client's slippage bound.

Ordinary orders pay no spread. Very large positions pay a fixed number of basis points on the part
that takes them past each size band, and spreads widen at times of stress, when venues diverge,
prices move fast, or the house's exposure runs high. Accounts whose trades are consistently
followed by a move in their favour, the mark of trading on prices that are about to change, pay a
small additional spread on every trade.

Losing trades pay nothing. Winning trades pay a fixed share of the profit they realise, and
positions held over the daily rollover pay or receive financing at a benchmark interest rate plus
or minus a markup.

Accounts are kept in US dollars. A market quoted in another currency, such as Germany 40 in euros
or Japan 225 in yen, accrues profit and loss in that currency and converts it to dollars at the
engine's own price for the currency pair, the same way for margin, exposure, and financing. While
that currency pair has no price, neither does the market, and nothing trades in it.

Margin is shared across an account's positions. When equity falls below half of the margin
requirement, the largest losing position that can trade is closed first, and so on until the
account recovers; a position in a market that is closed or has no fresh price waits until that
market prices again. Losses beyond an account's balance fall on the house, never on the trader. The
check runs on each price update, after each day's financing, and after a change to a margin table
or the close-out level. A market with no fresh price allows no dealing at all, while accounts
holding it trade other markets and withdraw against its last price. Markets in real-world assets
keep their exchange's hours, holidays, and early closes, and orders resting while a market is
closed fill at the first price after it opens, gap and all. The house limits its own risk by
capping its net exposure in each market, and the sum of those exposures across markets, at the
larger of a fraction of its net asset value and a fixed dollar floor, so trading can start from a
house with no capital and carries on while the house is in deficit.

Collateral is USDC, held in a vault contract on Arbitrum One. The chain safeguards funds and never
executes trades. A deposit is credited once the L2 batch holding it is posted to Ethereum
([#22](https://github.com/0xjevons/monkeytrading/issues/22)); withdrawals are being built in
[#23](https://github.com/0xjevons/monkeytrading/issues/23). Ownership of the house is represented
by the MONKEY token, minted to traders on their losing trades
([#36](https://github.com/0xjevons/monkeytrading/issues/36)). Holders who stake it share, in USDC,
the house's net asset value above a reserve floor
([#50](https://github.com/0xjevons/monkeytrading/issues/50)).

The operator lists markets and their trading calendars, sets benchmark rates and protocol
parameters, raises stress levels, and can stop new risk in one market or in all of them at once;
neither those modes nor any spread stops a trader reducing or closing a position in an open market
with a price. Once the engine has credited any funds, each
of these is a command the operator's key signs, published in the log with its signature, and replay
checks every one.

The reasoning behind each of these choices is recorded in the decision issues
[#1](https://github.com/0xjevons/monkeytrading/issues/1) to
[#16](https://github.com/0xjevons/monkeytrading/issues/16).

## Verifying the engine

The engine is deterministic: given the same sequence of commands, it reaches the same state on any
machine, to the last unit. The node streams each command it sequences, with the events it produced,
and the enclave periodically signs a root that commits to the whole state: a Merkle tree over
accounts, markets, and the rest of the state, so each checkpoint rehashes only what changed. Any
account can fetch a proof of its balance and positions against the last checkpoint's root and check
it without the node. `engine replay` re-applies a log from the beginning and checks the root at
every checkpoint against the state the published code computes, and fails a log it cannot find or
that holds no checkpoint; checking the signatures themselves arrives with
[#24](https://github.com/0xjevons/monkeytrading/issues/24). Replay reads a node's log directory or
the archive the node serves: its completed log segments, with a manifest of their seq ranges and
SHA-256 hashes. A stream subscriber can start or resume at any message offset, and gets the log from
disk and then live, each message once.

Every command carries the time the sequencer stamped it with. The enclave's host supplies its
clock, so the node checks that clock against signed time from several independent
[Roughtime](https://www.rfc-editor.org/rfc/rfc10049) servers. It stamps nothing until signed time
has arrived, and stops sequencing while the clock is further from the time the servers agree on
than a set bound, or once a minute has passed without signed time.

The enclave also attests to the key it signs with, and to the key its API serves TLS with: both are
generated inside the enclave, so client connections end where the host cannot read them. The local
node uses a mock attestation, labelled as such; attestation on AMD SEV-SNP hardware, and a verifier
for it, are being built in
[#26](https://github.com/0xjevons/monkeytrading/issues/26) and
[#27](https://github.com/0xjevons/monkeytrading/issues/27).

## Running it locally

You need the latest stable Rust on Linux or macOS, and `jq` and `xxd` for the API examples. Start
a local node with a mock enclave, mock custody, and simulated prices; its API is served over TLS on
`127.0.0.1:8080`, which you can change with `node.listen` in `config/dev.toml`. The certificate is
self-signed, so the examples below pass `curl -k`. Set `feeds.provider = "venues"` in a copy of the
config to price BTC-USD and ETH-USD from Coinbase, Kraken, and Bitstamp instead; the other markets
then have no price, so their orders get `no_price`. The node checks its clock against public
Roughtime servers over UDP: it starts sequencing once they answer, and stops a minute after it
loses them.

```bash
cargo test --workspace
cargo run -p engine-node -- run --config config/dev.toml
```

In another terminal, credit a test account, buy 0.25 BTC at the live price, and read the account
and its history:

```bash
API=https://localhost:8080
ACCOUNT=0x00000000000000000000000000000000000000aa
curl -sk -X POST $API/v1/dev/deposit -H 'content-type: application/json' \
  -d "{\"account\":\"$ACCOUNT\",\"amount\":\"50000\"}"
PRICE=$(curl -sk $API/v1/markets | jq -r '.[] | select(.symbol == "BTC-USD") | .mark')
curl -sk -X POST $API/v1/orders -H 'content-type: application/json' \
  -d "{\"account\":\"$ACCOUNT\",\"client_id\":1,\"market\":1,\"side\":\"buy\",\"qty\":\"0.25\",\"kind\":{\"kind\":\"market\",\"quoted\":\"$PRICE\"}}"
curl -sk $API/v1/accounts/$ACCOUNT
curl -sk "$API/v1/accounts/$ACCOUNT/history?limit=10"
```

`-k` only skips the CA check that a self-signed certificate cannot pass. What makes a connection
trustworthy is pinning the attested key: bytes 32..64 of the attestation's `report_data` are the
SHA-256 of the certificate's public key (its DER SubjectPublicKeyInfo), which is the form
`curl --pinnedpubkey` checks. Against a real enclave, verify the quote before trusting the bytes:

```bash
PIN=$(curl -sk $API/v1/attestation | jq -r '.report_data[66:]' | xxd -r -p | base64)
curl -s --insecure --pinnedpubkey "sha256//$PIN" $API/v1/health
```

The node's metrics are at `localhost:9464/metrics` in Prometheus format, and
[`ops/grafana/engine-node.json`](ops/grafana/engine-node.json) is a Grafana dashboard for them;
[`ops/prometheus/engine-node-rules.yml`](ops/prometheus/engine-node-rules.yml) holds alerting rules
for a node that goes silent or cannot deliver its pages. To have Critical alerts posted to a
webhook, add an `[alerts.critical]` section naming the environment variable that holds its URL, as
`config/dev.toml` shows.

Stopping the node with ctrl-c lets it finish what is queued and write a final checkpoint. You can
then verify the log it wrote:

```bash
cargo run -p engine-node -- replay --log data/dev/log
```

The local configuration accepts unsigned requests (`auth = "none"`). With `auth = "eip712"`, each
order, cancel, and withdrawal must carry its account's EIP-712 signature, its next nonce, and an
expiry at most five minutes ahead, and replay checks every one. The first start records this
setting in the log, so changing it later needs a fresh log: with the node stopped, remove
`data/dev/log`, and the node rebuilds the account history index in `data/dev/history` from the new
one. The node's full API is described in [`crates/node/README.md`](crates/node/README.md).

As the operator, `engine admin` signs a command and submits it over TLS pinned to the node's
attested key. The local configuration names the public key of a test key, so on a local node this
stops new risk in every market:

```bash
PIN=$(curl -sk https://127.0.0.1:8080/v1/attestation | jq -r .tls_public_key_sha256)
OPERATOR_KEY=0x4242424242424242424242424242424242424242424242424242424242424242 \
  cargo run -p engine-node -- admin --pin $PIN set-reduce-only true
```

## Repository layout

The workspace has two crates. [`crates/core`](crates/core) (`engine-core`) is the deterministic
state machine: it performs no input or output, and uses neither clocks, randomness, nor
floating-point arithmetic. [`crates/node`](crates/node) (`engine-node`, binary `engine`) runs it,
sequencing commands, writing and streaming the log, signing checkpoints, serving the API, and
connecting to custody and price feeds. Node configuration lives in [`config`](config), operator
assets such as the Grafana dashboard in [`ops`](ops), and each crate's README describes its files.
[`contracts`](contracts) is a Foundry project holding the custody vault, and
[`scripts/dev-vault-deposits.sh`](scripts/dev-vault-deposits.sh) runs a node against it on a local
Anvil chain.

## Contributing

Work is tracked in GitHub issues, and pull requests against `main` are welcome. CI checks
formatting, file length, lints, tests, and dependency advisories and licences, replays the
committed reference logs on x86_64 and aarch64 Linux, and checks the contracts' formatting, build,
and tests and the vault deposit scenario; run the same checks locally before opening a pull
request. The code's conventions are documented in [`agents/AGENTS.md`](agents/AGENTS.md).

## Security

Please report vulnerabilities privately to william.jevons@proton.me rather than in a public issue.
The [security policy](SECURITY.md) explains what a report should include, how quickly to expect a
response, how disclosure is coordinated, and which components are in scope. A bug bounty will be
announced before launch.

## License

monkeytrading is licensed under the Apache License, Version 2.0; see [`LICENSE`](LICENSE).
