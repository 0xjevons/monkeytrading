# monkeytrading

An open-source engine for contracts for difference (CFDs) on crypto, FX, indices, commodities, and
shares. The house is the counterparty to every trade, and its equity is issued to the people who
trade with it. The engine runs inside a confidential-computing enclave and publishes every command
it sequences, so anyone can replay the public log and check the state roots the enclave signs.

> **Status: pre-launch.** The engine and node run locally with a mock enclave, mock custody, and
> mock price feeds. Real custody, hardware attestation, live market data, and the house equity
> token are being built; see [Roadmap](#roadmap). No deployment holds real funds.

## How it works

The protocol as designed. Parts still being built link to their issue.

- **Execution.** Orders fill immediately against the median price of several independent venues.
  When too few venue quotes are fresh, or the fresh ones disagree, the market has no price and
  nothing fills.
- **Pricing.** Normal size fills at the index mid. Very large positions pay fixed basis-point
  spreads by size band. Stress levels widen spreads automatically on venue divergence, fast moves,
  or house exposure.
- **Quotes.** A quote is indicative: a market order fills at the live price, and is declined only
  when that price is worse than the client's quote by more than a slippage bound.
- **Fees.** Losing trades pay nothing. Winning trades pay a flat percentage of realised profit.
  Positions held over the daily rollover pay or receive financing at a benchmark rate plus or
  minus a markup.
- **Margin.** Margin is shared across an account. Below 50% of the margin requirement, the largest
  losing position is closed first. Losses beyond an account's balance are written off against the
  house.
- **Market hours.** Real-world-asset markets keep their venue's hours. Nothing fills while a market
  is closed; resting orders fill at the first price after the open, gaps included.
- **House risk.** The house's net exposure is capped per market and in total, as fractions of
  house NAV ([#45](https://github.com/0xjevons/monkeytrading/issues/45) adds a dollar floor).
- **Custody.** Collateral is USDC in a vault contract on Arbitrum One; the chain holds funds and
  never executes trades ([#22](https://github.com/0xjevons/monkeytrading/issues/22),
  [#23](https://github.com/0xjevons/monkeytrading/issues/23)).
- **House equity.** The MONKEY token is minted to traders on losing trades, and stakers receive
  USDC distributions of house NAV above a floor
  ([#36](https://github.com/0xjevons/monkeytrading/issues/36),
  [#50](https://github.com/0xjevons/monkeytrading/issues/50)).
- **Public log.** Every sequenced command is published with the events it produced, and the
  enclave signs periodic state roots. `engine replay` re-applies a log and checks every root.

Each design decision and its reasoning is recorded in the closed `Decision:` issues
([#1](https://github.com/0xjevons/monkeytrading/issues/1) to
[#16](https://github.com/0xjevons/monkeytrading/issues/16)).

## Repository layout

| Path | Contents |
| --- | --- |
| [`crates/core`](crates/core) | `engine-core`, the deterministic state machine. No IO, clocks, randomness, or floats. |
| [`crates/node`](crates/node) | `engine-node`, binary `engine`: sequencer, public log and checkpoints, REST and WebSocket API, attestation, custody and feed adapters. |
| [`config`](config) | Node configs. `dev.toml` runs everything locally with mocks. |
| [`agents`](agents) | Rules and skills for the AI agents that build this repository ([`agents/AGENTS.md`](agents/AGENTS.md)). |
| [`scripts`](scripts) | Repository checks, such as the 500-line cap per Rust file. |

Each crate's README lists its files and API.

## Quick start

Requires the latest stable Rust; the second example also uses `jq`.

```bash
cargo test --workspace

# A local node with a mock enclave, custody, and feeds. The API listens on
# 127.0.0.1:8080; change node.listen in config/dev.toml if that port is taken.
cargo run -p engine-node -- run --config config/dev.toml
```

In another terminal, credit a test account (mock custody only) and buy 0.25 BTC at the live price:

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

Stop the node with ctrl-c: it drains and writes a final checkpoint. Then verify the log it wrote:

```bash
cargo run -p engine-node -- replay --log data/dev/log.jsonl
```

The dev config accepts unsigned requests; signed client requests are
[#18](https://github.com/0xjevons/monkeytrading/issues/18). The full API is listed in
[`crates/node/README.md`](crates/node/README.md).

## Roadmap

The work to public launch is planned as GitHub issues in
[milestones](https://github.com/0xjevons/monkeytrading/milestones) M1 to M9. Features are built in
waves, labelled `wave-1` to `wave-6`: each wave's pull requests branch from `main` once every
feature they depend on has merged. The
[current wave](https://github.com/0xjevons/monkeytrading/issues?q=is%3Aopen+label%3Awave-1) lists
what is being built now.

## Contributing

- Branch from `main` and open pull requests as drafts.
- CI runs formatting, the file line cap, clippy, tests, and `cargo-deny`
  ([`.github/workflows/ci.yaml`](.github/workflows/ci.yaml)); run the same checks locally.
- A pull request is marked ready only after a multi-agent review leaves no Critical, High, or
  Medium findings ([`agents/skills/agent-create-pr`](agents/skills/agent-create-pr/SKILL.md)).
- Code conventions are in [`agents/AGENTS.md`](agents/AGENTS.md) and the Rust skill it links.

## Security

Report vulnerabilities privately to william.jevons@proton.me. A security policy and a bug bounty
are planned before launch ([#43](https://github.com/0xjevons/monkeytrading/issues/43)).

## License

Licensed under the Apache License, Version 2.0; see [`LICENSE`](LICENSE).
