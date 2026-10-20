# engine-node

## Overview

The node that runs `engine-core` inside a TEE (an Intel TDX or AMD SEV-SNP confidential VM). It
sequences every command into a public, append-only log, signs state-root checkpoints with an
attested enclave key, serves the client API, streams the log live, and talks to custody and price
feeds. The same binary replays a published log and verifies its checkpoints.

## Features

- Single-task sequencer: commands are logged and flushed before any client sees their effect
- Signed checkpoints and withdrawals, bound to the chain id and vault, from an attested key
- Attestation through Linux configfs-tsm, covering TDX and SEV-SNP, or a clearly marked mock
- REST API for orders, cancels, withdrawals, and reads, plus a WebSocket stream of the log
- `engine replay` to verify a published log against the open-source engine
- Supervised tasks: any task stopping stops the node, and SIGTERM drains to a final checkpoint
- Mock custody and price feeds for local development

## Repository Structure

- `src/main.rs` — the `engine` binary: `run` and `replay`
- `src/lib.rs` — module index
- `src/node.rs` — startup, task supervision, and shutdown
- `src/sequencer.rs` — the sequencer, checkpoints, alerts, and log replay
- `src/log.rs` — the JSON-lines log format, writer, and reader
- `src/api.rs` — REST routes, the request span, and the WebSocket stream
- `src/tee.rs` — the enclave key, signatures, and attestation providers
- `src/config.rs` — the node config schema and validation
- `src/custody.rs` — the custody adapter (mock only so far)
- `src/feeds.rs` — price feed adapters (mock only so far)
- `src/clock.rs` — the wall clock used to stamp commands

## Usage

```bash
# Run a local node with mock TEE, custody, and feeds (API on 127.0.0.1:8080)
cargo run -p engine-node -- run --config config/dev.toml

# Verify a published log
cargo run -p engine-node -- replay --log data/dev/log.jsonl
```

Endpoints: `GET /v1/health`, `GET /v1/attestation`, `GET /v1/markets`, `GET /v1/house`,
`GET /v1/accounts/{address}`, `POST /v1/orders`, `POST /v1/orders/cancel`, `POST /v1/withdrawals`,
`POST /v1/dev/deposit` (mock custody only), and `GET /v1/stream` (WebSocket).

## Configuration

`CONFIG_PATH`: path to the node config file, used when `run` is given no config flag.

## Testing

```bash
cargo test -p engine-node
```

## Authors

The monkeytrading contributors

## License

Licensed under the Apache License, Version 2.0; see `LICENSE`.
