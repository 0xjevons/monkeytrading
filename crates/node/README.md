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
- Orders, cancels, and withdrawals signed by the account (EIP-712), checked before they are
  sequenced; `auth = "none"` trusts the account field and runs only with the mock TEE and custody
- The latest protocol version and the client auth activated at boot, at the log's next seq
- Stream offsets of `<seq>:<index>`: a subscriber starts or resumes at any offset, reads the log
  from disk, then goes live, getting each message once; one that falls behind resumes from disk
- Segmented log: completed segments and a manifest of their seq ranges and SHA-256 hashes are a
  public archive served over HTTP
- `engine replay` to verify a published log, or a downloaded archive, against the open-source engine
- Supervised tasks: any task stopping stops the node, and SIGTERM drains to a final checkpoint
- Live crypto prices from Coinbase, Kraken, and Bitstamp over WebSocket: one supervised task per
  venue, quotes stamped with the venue's own time, and reconnects with backoff
- Alerts for refused venue messages (at most one a minute per venue, counting the rest), venue
  outages, and symbols with no fresh quote
- Mock custody and a mock price feed for local development

## Repository Structure

- `src/main.rs` — the `engine` binary: `run` and `replay`
- `src/lib.rs` — module index
- `src/node.rs` — startup, task supervision, and shutdown
- `src/sequencer/mod.rs` — the sequencer, protocol activation, genesis, stream messages,
  checkpoints, alerts, and log replay
- `src/sequencer/tests.rs` — sequencer, activation, genesis restart, and replay tests
- `src/log/mod.rs` — the JSON-lines log format, stream offsets, and the single-file reader
- `src/log/segments.rs` — segment files, rotation, the manifest, and recovery checks
- `src/log/reader.rs` — reads the log from any offset while it is being written
- `src/log/checked.rs` — reads a completed segment in chunks checked against its listing
- `src/log/tests.rs` — log format, segment, and reader tests
- `src/api/mod.rs` — REST routes, request signature checks, and the request span
- `src/api/stream.rs` — the WebSocket stream: backfill from disk, then live
- `src/api/archive.rs` — the archive routes: the manifest and completed segments
- `src/api/auth_tests.rs` — signed-request API tests: signatures, nonces, expiries, refused
  accounts, and racing requests
- `src/api/tests.rs` — API tests, including the resumable stream and archive dev scenario
- `src/test_support.rs` — the shared test harness
- `src/tee.rs` — the enclave key, signatures, and attestation providers
- `src/config.rs` — the node config schema and validation
- `src/custody.rs` — the custody adapter (mock only so far)
- `src/feeds/mod.rs` — the feed index and the mock feed
- `src/feeds/venue.rs` — the venue task: connect, subscribe, sequence quotes, alert, reconnect
- `src/feeds/coinbase.rs`, `src/feeds/kraken.rs`, `src/feeds/bitstamp.rs` — venue adapters
- `src/feeds/tests/` — venue tasks against loopback venues, and the dev scenario through the
  sequencer
- `src/clock.rs` — the wall clock used to stamp commands
- `examples/reference_log.rs` — writes a protocol version's node reference log from engine-core's
- `tests/reference_logs.rs` — replays every committed node reference log, events included
- `tests/reference/` — one node-format reference log per protocol version, never regenerated
- `tests/replay.rs` — replays a log the node wrote before protocol versions, kept in
  `tests/fixtures/`

## Usage

```bash
# Run a local node with mock TEE, custody, and feeds (API on 127.0.0.1:8080)
cargo run -p engine-node -- run --config config/dev.toml

# Price BTC-USD and ETH-USD from live venues instead: set feeds.provider = "venues" in a copy
cargo run -p engine-node -- run --config <copy of config/dev.toml>

# Verify a node's log directory, a downloaded archive, or a single log file
cargo run -p engine-node -- replay --log data/dev/log

# Download the archive: the manifest, then each segment it lists
curl --create-dirs -o archive/manifest.jsonl http://127.0.0.1:8080/v1/archive/manifest.jsonl
jq -r '.file' archive/manifest.jsonl |
  xargs -I{} curl -o archive/{} http://127.0.0.1:8080/v1/archive/{}
```

Endpoints: `GET /v1/health`, `GET /v1/attestation`, `GET /v1/markets`, `GET /v1/house`,
`GET /v1/accounts/{address}`, `GET /v1/accounts/{address}/nonce`, `POST /v1/orders`,
`POST /v1/orders/cancel`, `POST /v1/withdrawals`, `POST /v1/dev/deposit` (mock custody only),
`GET /v1/stream` (WebSocket), and `GET /v1/archive/{file}` (`manifest.jsonl` or a segment it lists).

`/v1/stream` is live from now by default. `?from=<seq>` starts at the first message of `seq`
(`from=0` is the whole log), and `?after=<seq>:<index>` resumes after the `offset` of the last
message received. Each message carries its `offset`: the command of a seq is index 0, then its
withdrawals, then any checkpoint. A command logged before events were recorded in the log streams
without its `events` key.

The log is a directory (`node.log_dir`) of segment files named for their first seq, plus
`manifest.jsonl`, which gets one line per completed segment with its seq range, size, and SHA-256.
The node loads the manifest at recovery, after checking it against the segment files, and then holds
it in memory, adding each segment it completes. The archive serves that copy, not the file on disk.
Backfill reads up to 256 messages at a time and keeps them until the subscriber takes them. It
checks a completed segment against the manifest in one pass the first time it reads from it, then
checks each chunk it reads, and refuses any other segment the manifest does not list. The segment
still being written is read from disk unchecked, and replay against the signed checkpoints is the
check for it. While it reads a batch, a subscriber holds one of 16 permits and one 1 MiB chunk of a
segment. If a new segment cannot be created, the current one keeps growing and the node warns
(`log_rotation_failed`) until it can. A log written as one file before segments existed continues as
the first segment: move it to `<log_dir>/00000000000000000000.jsonl`. Every segment is kept, since
recovery replays the log from genesis.

With `auth = "eip712"`, each order, cancel, and withdrawal body carries
`"auth": { "nonce": <next nonce>, "expires_at": <ms>, "signature": "0x…" }`: the account's EIP-712
signature under the domain of `custody.vault` on `custody.chain_id` (see `engine-core`'s
`sign_request` example). `expires_at` is the latest time, in milliseconds since the Unix epoch, at
which the request itself is accepted; a limit or stop order it places rests until it fills or is
cancelled. A missing or invalid signature is refused with 401, an expired request with 400, a
signed request from an account that has never been credited with 403, and a nonce other than the
account's next with 409 and the `expected` nonce; none of these reaches the log. A request
refused with 403 or 409 uses no nonce, so it stays valid until its `expires_at`: resubmit that same
request once its nonce is next, or let it expire before signing different content at that nonce.
An expiry more than five minutes ahead is sequenced and rejected with `expiry_too_far`, which uses
the nonce. Two requests that race for one nonce are both sequenced, and the engine rejects the
second with `invalid_nonce`.

The first start records `auth`, and under `eip712` the custody chain id and vault, in the log;
changing any of them needs a fresh log.

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
