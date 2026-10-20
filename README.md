# monkeytrading

monkeytrading is an open-source engine for trading contracts for difference on crypto-assets,
currencies, stock indices, commodities, and shares. As with a conventional CFD broker, the house
takes the other side of every trade. Unlike one, its rules are public code, it runs inside a
confidential-computing enclave, and it publishes every command it processes, so anyone can replay
the record and confirm that the state it reports is the state its rules produce. Ownership of the
house, in turn, is issued to the people who trade with it.

> **Pre-launch.** The engine runs end to end on a single machine, with a mock enclave, mock
> custody, and simulated prices. On-chain custody, hardware attestation, live market data, and the
> house equity token are under construction. Nothing here holds real funds.

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

Margin is shared across an account's positions. When equity falls below half of the margin
requirement, the largest losing position is closed first, and so on until the account recovers.
Losses beyond an account's balance fall on the house, never on the trader. The check runs on each
price update today; [#45](https://github.com/0xjevons/monkeytrading/issues/45) extends it to
financing and margin changes. Markets in real-world assets keep their exchange's hours, and orders
resting while a market is closed fill at the first price after it opens, gap and all. The house
limits its own risk by capping its net exposure in each market, and the sum of those exposures
across markets, as fractions of its net asset value.

Collateral is USDC, held in a vault contract on Arbitrum One. The chain safeguards funds and never
executes trades; deposits are being built in
[#22](https://github.com/0xjevons/monkeytrading/issues/22) and withdrawals in
[#23](https://github.com/0xjevons/monkeytrading/issues/23). Ownership of the house is represented
by the MONKEY token, minted to traders on their losing trades
([#36](https://github.com/0xjevons/monkeytrading/issues/36)). Holders who stake it share, in USDC,
the house's net asset value above a reserve floor
([#50](https://github.com/0xjevons/monkeytrading/issues/50)).

The reasoning behind each of these choices is recorded in the decision issues
[#1](https://github.com/0xjevons/monkeytrading/issues/1) to
[#16](https://github.com/0xjevons/monkeytrading/issues/16).

## Verifying the engine

The engine is deterministic: given the same sequence of commands, it reaches the same state on any
machine, to the last unit. The node streams each command it sequences, with the events it produced,
and the enclave periodically signs a hash of the whole state. `engine replay` re-applies a log from
the beginning and checks the hash at every checkpoint against the state the published code
computes; checking the signatures themselves arrives with
[#24](https://github.com/0xjevons/monkeytrading/issues/24). Replay reads the node's log file today,
and a downloadable archive and a resumable stream are being built in
[#20](https://github.com/0xjevons/monkeytrading/issues/20).

The enclave also attests to the key it signs with. The local node uses a mock attestation, labelled
as such; attestation on AMD SEV-SNP hardware, and a verifier for it, are being built in
[#26](https://github.com/0xjevons/monkeytrading/issues/26) and
[#27](https://github.com/0xjevons/monkeytrading/issues/27).

## Running it locally

You need the latest stable Rust on Linux or macOS, and `jq` for the second example. Start a local
node with a mock enclave, mock custody, and simulated prices; its API listens on `127.0.0.1:8080`,
which you can change with `node.listen` in `config/dev.toml`.

```bash
cargo test --workspace
cargo run -p engine-node -- run --config config/dev.toml
```

In another terminal, credit a test account and buy 0.25 BTC at the live price:

```bash
API=localhost:8080
ACCOUNT=0x00000000000000000000000000000000000000aa
curl -s -X POST $API/v1/dev/deposit -H 'content-type: application/json' \
  -d "{\"account\":\"$ACCOUNT\",\"amount\":\"50000\"}"
PRICE=$(curl -s $API/v1/markets | jq -r '.[] | select(.symbol == "BTC-USD") | .mark')
curl -s -X POST $API/v1/orders -H 'content-type: application/json' \
  -d "{\"account\":\"$ACCOUNT\",\"client_id\":1,\"market\":1,\"side\":\"buy\",\"qty\":\"0.25\",\"kind\":{\"kind\":\"market\",\"quoted\":\"$PRICE\"}}"
curl -s $API/v1/accounts/$ACCOUNT
```

Stopping the node with ctrl-c lets it finish what is queued and write a final checkpoint. You can
then verify the log it wrote:

```bash
cargo run -p engine-node -- replay --log data/dev/log.jsonl
```

The local configuration accepts unsigned requests; signed requests are being built in
[#18](https://github.com/0xjevons/monkeytrading/issues/18). The node's full API is described in
[`crates/node/README.md`](crates/node/README.md).

## Repository layout

The workspace has two crates. [`crates/core`](crates/core) (`engine-core`) is the deterministic
state machine: it performs no input or output, and uses neither clocks, randomness, nor
floating-point arithmetic. [`crates/node`](crates/node) (`engine-node`, binary `engine`) runs it,
sequencing commands, writing and streaming the log, signing checkpoints, serving the API, and
connecting to custody and price feeds. Node configuration lives in [`config`](config), and each
crate's README describes its files.

## Contributing

Work is tracked in GitHub issues, and pull requests against `main` are welcome. CI checks
formatting, file length, lints, tests, and dependency advisories and licences; run the same checks
locally before opening a pull request. The code's conventions are documented in
[`agents/AGENTS.md`](agents/AGENTS.md).

## Security

Please report vulnerabilities privately to william.jevons@proton.me. A security policy and a bug
bounty will be published before launch.

## License

monkeytrading is licensed under the Apache License, Version 2.0; see [`LICENSE`](LICENSE).
