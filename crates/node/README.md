# engine-node

## Overview

The node that runs `engine-core` inside a TEE (an Intel TDX or AMD SEV-SNP confidential VM). It
sequences every command into a public, append-only log, signs state-root checkpoints with an
attested enclave key, serves the client API, streams the log live, indexes each account's history
from the log, and talks to custody and price feeds. The same binary replays a published log and
verifies its checkpoints.

## Features

- Single-task sequencer: commands are logged and flushed before any client sees their effect
- Signed checkpoints and withdrawals, bound to the chain id and vault, from an attested key
- Attestation through Linux configfs-tsm, covering TDX and SEV-SNP, or a clearly marked mock
- Trusted time: every stamp is checked against signed Roughtime readings that independent servers
  agree on, and nothing, genesis included, is stamped before the first; sequencing halts with a
  Critical alert when the host clock is beyond the drift bound, or 60 seconds after the last
  sample, and resumes once a sample is within the bound
- REST API for orders, cancels, withdrawals, and reads, plus a WebSocket stream of the log
- Orders, cancels, and withdrawals signed by the account (EIP-712), checked before they are
  sequenced; `auth = "none"` trusts the account field and runs only with the mock TEE and custody
- The latest protocol version and the client auth activated at boot, at the log's next seq
- Stream offsets of `<seq>:<index>`: a subscriber starts or resumes at any offset, reads the log
  from disk, then goes live, getting each message once; one that falls behind resumes from disk
- Segmented log: completed segments and a manifest of their seq ranges and SHA-256 hashes are a
  public archive served over HTTP
- TLS terminated in the enclave, with a key generated there and its hash in the attestation
- Rate limits on requests per client IP and per account and on new connections per client IP,
  caps on request size, connections, and stream subscribers, and read timeouts, all set in `[api]`
- `engine replay` to verify a published log, or a downloaded archive, against the open-source engine
- Account history: every event that names an account, indexed in SQLite from the log and paged
  newest first by a `<seq>:<index>` cursor, with a stated retention; it survives restarts and
  rebuilds from the log
- Supervised tasks: any task stopping stops the node, and SIGTERM drains to a final checkpoint
- Live crypto prices from Coinbase, Kraken, and Bitstamp over WebSocket: one supervised task per
  venue, quotes stamped with the venue's own time, and reconnects with backoff
- Alerts for refused venue messages (at most one a minute per venue, counting the rest), venue
  outages, symbols with no fresh quote, and venue times well ahead of the host clock
- A `house_deficit` alert when house NAV falls below zero, at most one a minute while it keeps
  crossing zero
- EVM custody: each vault deposit credited once, after a configurable finality tag and
  confirmations, with a cursor that survives restarts
- Mock custody and a mock price feed for local development

## Repository Structure

- `src/main.rs` — the `engine` binary: `run` and `replay`
- `src/lib.rs` — module index
- `src/node.rs` — startup, task supervision, and shutdown
- `src/sequencer/mod.rs` — the sequencer, stream messages, checkpoints, and log replay
- `src/sequencer/required.rs` — protocol activation and genesis, which wait for trusted time, and
  the genesis commands
- `src/sequencer/alerts.rs` — the alerts the sequencer raises: engine rejections an operator must
  look at, and house NAV below zero
- `src/sequencer/tests.rs` — sequencer, activation, genesis restart, and replay tests
- `src/sequencer/margin_tests.rs` — the margin and solvency dev scenario on the dev genesis (a
  close-out after the rollover's financing, and a withdrawal limited at a stale market's last
  mark), and a genesis with no house capital that trades and alerts on a house deficit
- `src/log/mod.rs` — the JSON-lines log format, stream offsets, and the single-file reader
- `src/log/segments.rs` — segment files, rotation, the manifest, and recovery checks
- `src/log/reader.rs` — reads the log from any offset while it is being written
- `src/log/checked.rs` — reads a completed segment in chunks checked against its listing
- `src/log/tests.rs` — log format, segment, and reader tests
- `src/api/mod.rs` — REST routes, request signature checks, and the request span
- `src/api/stream/mod.rs` — the WebSocket stream: backfill from disk, then live, under the
  subscriber cap, reading what the subscriber sends
- `src/api/stream/tests.rs` — feed, backfill, and subscriber tests
- `src/api/archive.rs` — the archive routes: the manifest and completed segments
- `src/api/auth_tests.rs` — signed-request API tests: signatures, nonces, expiries, refused
  accounts, and racing requests
- `src/api/history.rs` — the account history route: its parameters and limits
- `src/api/tests.rs` — API tests, including the dev scenarios of the resumable stream and archive,
  and of account history, and `Retry-After`
- `src/history/mod.rs` — the history index: which events name an account, the startup check
  against the log, and the supervised task that follows the log into the index
- `src/history/store.rs` — the index's SQLite schema, its writer, and its page reader
- `src/history/tests/mod.rs` — history index tests: pages, filters, retention, following the log,
  and restarts
- `src/history/tests/recovery.rs` — startup checks against the log, readiness to serve, and
  failures
- `src/test_support.rs` — the shared test harness
- `src/tee.rs` — the enclave key, signatures, and attestation providers
- `src/trusted_time/mod.rs` — the trusted time task: queries the Roughtime servers, backs off
  failing ones, and turns agreeing replies into a sample
- `src/trusted_time/gate.rs` — the sequencer's check of each stamp against the latest sample
- `src/trusted_time/tests/` — agreement, round, back-off, and gate tests, and the offline dev
  scenario
- `src/roughtime/mod.rs` — the Roughtime client: builds requests and verifies signed replies, for
  version 1 (RFC 10049) and the final drafts' 0x8000000c
- `src/roughtime/message.rs` — the Roughtime wire format
- `src/roughtime/testing.rs` — a Roughtime server for tests that signs with its own keys
- `src/roughtime/tests.rs` — client tests over recorded and locally signed replies
- `src/roughtime/fixtures/replies.json` — replies recorded from the servers in `config/dev.toml`
- `src/tls/mod.rs` — the TLS key and certificate, and the server loop that applies the connection
  caps and timeouts
- `src/tls/tests/mod.rs` — the API end to end over TLS: key pinning, limits, caps, and the harness
- `src/tls/tests/stream.rs` — the stream end to end over TLS: the subscriber and message caps
- `src/limits.rs` — request rate limits, connection caps, and the stream subscriber cap
- `src/config/mod.rs` — the node config schema and loading
- `src/config/custody.rs` — the `[custody.evm]` section, the redacted RPC URL, and the custody
  checks
- `src/config/tests.rs` — config loading and validation tests
- `src/config/validation.rs` — the checks a parsed config must also pass
- `src/custody/mod.rs` — the custody providers (mock and EVM) and the USDC-to-`Fixed` conversion
- `src/custody/evm.rs` — the deposit watcher: the chain and token check, range reads from blocks
  proven to lead to the tagged final block, the completeness check, and stall alerts
- `src/custody/chain.rs` — the chain of block hashes proven to lead to the tagged block, and the
  headers of an interrupted proof, kept apart until it ends there
- `src/custody/watchdog.rs` — alerts for a stale endpoint head, a final block that stops, and
  reads that keep trailing the final block
- `src/custody/stall.rs` — what a poll did, the watcher's run loop, and the alert each kind of
  stall raises
- `src/custody/sizing.rs` — read sizes that adapt to a provider's limits
- `src/custody/credit.rs` — crediting one deposit once, and the sequencer round trips it needs
- `src/custody/rpc.rs` — the vault over JSON-RPC, trusting only compiled-in TLS roots, with
  errors scrubbed of the endpoint URL
- `src/custody/cursor.rs` — the deposit cursor kept between restarts
- `src/custody/tests/mod.rs` — deposit watcher tests against a fake chain and a real sequencer
- `src/custody/tests/alerts.rs` — the watcher's alerts over time, in paused time
- `src/custody/tests/cursor.rs` — the deposit cursor across restarts, and which cursors a log
  refuses
- `src/custody/tests/fakes.rs` — the fake chain, test sequencer, log capture, and fixtures
- `src/custody/tests/rpc.rs` — the RPC adapter's log decoding, deadlines, and URL scrubbing
- `src/custody/tests/reorg.rs` — reorgs and forked answers against the proven chain
- `src/feeds/mod.rs` — the feed index and the mock feed
- `src/feeds/venue/mod.rs` — the venue task: connect, subscribe, sequence quotes, alert, reconnect
- `src/feeds/venue/validation.rs` — the checks every venue's books pass before they are sequenced
- `src/feeds/venue/confirm.rs` — confirming with the engine which markets the venue is quoting,
  holding back markets it does not list, and settling a dropped connection's last quotes
- `src/feeds/venue/quotes.rs` — the quotes held between ticks until the host clock reaches them,
  and the rule that decides whether the quote the engine holds counts as the venue quoting
- `src/feeds/venue/setup.rs` — the TLS client config and reconnect backoff every connection shares
- `src/feeds/venue/tests.rs` — venue task unit tests: alert throttling and silence windows on
  paused time, held quotes against the engine's rule, kept books for unsubscribed symbols, the
  freshness rule, and backoff
- `src/feeds/coinbase.rs`, `src/feeds/bitstamp.rs` — venue adapters
- `src/feeds/kraken/mod.rs` — the Kraken adapter: a kept order book verified against Kraken's
  checksum; `src/feeds/kraken/tests.rs` replays recorded book messages against it
- `src/feeds/tests/mod.rs` — venue tasks against loopback venues: what they sequence, refuse,
  and alert, and how they reconnect
- `src/feeds/tests/fake.rs` — the loopback venues, the feed harness with an engine that applies
  what the feed sequences, and the alert capture the feed tests use
- `src/feeds/tests/late.rs` — venue tasks against a sequencer that answers late: when a quote
  counts as the venue quoting, and how a connection's last quotes are settled after it ends
- `src/feeds/tests/scenario.rs` — the dev scenario through the sequencer and engine
- `src/throttle.rs` — holds a repeating alert to one a minute, counting the ones it held back
- `src/clock.rs` — the wall and monotonic clocks the sequencer stamps and checks with
- `examples/reference_log.rs` — writes a protocol version's node reference log from engine-core's
- `tests/reference_logs.rs` — replays every committed node reference log, events included
- `tests/reference/` — one node-format reference log per protocol version, never regenerated
- `tests/replay.rs` — replays a log the node wrote before protocol versions, kept in
  `tests/fixtures/`
- `tests/production_genesis.rs` — loads `config/production-genesis.toml` as a dev node's genesis,
  checks the launch values, and steps it through weeks with holidays and early closes

## Usage

```bash
# Run a local node with mock TEE, custody, and feeds (API on https://127.0.0.1:8080)
cargo run -p engine-node -- run --config config/dev.toml

# Call it pinned to the attested TLS key: report data bytes 32..64 are the SHA-256 of the
# certificate's DER SubjectPublicKeyInfo
API=https://127.0.0.1:8080
PIN=$(curl -sk $API/v1/attestation | jq -r '.report_data[66:]' | xxd -r -p | base64)
curl -s --insecure --pinnedpubkey "sha256//$PIN" $API/v1/health

# Price BTC-USD and ETH-USD from live venues instead, leaving the other markets unpriced: set
# feeds.provider = "venues" in a copy
cargo run -p engine-node -- run --config <copy of config/dev.toml>

# Verify a node's log directory, a downloaded archive, or a single log file
cargo run -p engine-node -- replay --log data/dev/log

# Download the archive: the manifest, then each segment it lists (`--retry` waits out a 429)
curl -kf --retry 5 --create-dirs -o archive/manifest.jsonl https://127.0.0.1:8080/v1/archive/manifest.jsonl
jq -r '.file' archive/manifest.jsonl |
  xargs -I{} curl -kf --retry 5 -o archive/{} https://127.0.0.1:8080/v1/archive/{}

# Run the vault deposit scenario: Anvil, the vault, and a node with EVM custody
./scripts/dev-vault-deposits.sh
```

EVM custody reads the vault through `custody.evm.rpc_url`, which must use HTTPS unless it is on
this machine and must serve state at past blocks back to `custody.evm.start_block` (an archive
node): catching up after downtime and rescanning both read past blocks.

A vault deposit credits the trading account its event names: the caller for `deposit`, or the
account `depositFor` names. The vault refuses deposits under 10 USDC or over 1e9 USDC. A trading
account must be able to sign requests with its own key, since the engine accepts no contract
signatures: a contract wallet funds a key that signs for it through `depositFor`, and a balance
credited to a contract account can be recovered only through the exit (#48).

The adapter trusts its RPC endpoint's answers as far as the chain of block hashes it proves up to
the finality tag, and the vault's deposit count, bound them. It takes the vault, its token, and the
finality tag from the host-supplied config, and keeps its cursor on host storage. Reorganisations
of Arbitrum One that undo a block after its batch reached Ethereum, the finality it waits for, are
outside its threat model.

Endpoints: `GET /v1/health`, `GET /v1/attestation`, `GET /v1/markets`, `GET /v1/house`,
`GET /v1/accounts/{address}`, `GET /v1/accounts/{address}/nonce`,
`GET /v1/accounts/{address}/history`, `POST /v1/orders`, `POST /v1/orders/cancel`,
`POST /v1/withdrawals` (mock custody only until withdrawals are paid on-chain), `POST /v1/dev/deposit`
(mock custody only), `GET /v1/stream` (WebSocket), and
`GET /v1/archive/{file}` (`manifest.jsonl` or a segment it lists).
A client over its rate gets 429 with `Retry-After`, a body over `api.max_body_bytes` gets 413, a
body not received within `api.read_timeout` gets 408, and a subscriber over `api.max_subscribers`
gets 503. A connection over a connection cap or over its client's connection rate is closed before
its TLS handshake. A client is its socket peer IP, with IPv6 counted by its /64; forwarding headers
are ignored.

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
changing any of them needs a fresh log, and with it a fresh `history.dir`, both removed while the
node is stopped.

`/v1/accounts/{address}/history` returns `{"events": [...], "next": ...}`: the account's events,
newest first. Each event carries its `cursor`, `<seq>:<index>`, which is the seq of the command that
produced it and its index among that command's events; then `ts`, the command's sequenced time; then
the event's own fields. `?before=<cursor>` and `?after=<cursor>` bound the page, both exclusive.
Passing a page's `next` as `before` gives the next, older page, until `next` is null. `?kind=` takes
one event `type`: `deposited`, `deposit_rejected`, `withdrawal_approved`, `withdrawal_rejected`,
`order_accepted`, `order_rejected`, `order_cancelled`, `fill`, `close_out`, `bad_debt`, or
`financing`. `?limit=` is 1 to 200, 50 by default. A page is one search of a SQLite index that reads
at most `limit + 1` entries, and at most 16 pages are read at once. Until the index has taken every
command the log held when the node started, the route answers 503 (`history is catching up with the
log`). History is public, like the log.

The history index is a SQLite file in `history.dir`, derived from the log alone. A supervised task
follows the log as a stream subscriber does, from disk and then live, and writes on its own thread,
so the sequencer never waits for it; once caught up, history trails the log by the commands queued
for the writer, and reads from disk whatever it falls behind the live log by. Each write stores the
account events of a batch of commands, keyed by cursor, and the last command taken, in one
transaction. Events whose command is older than `history.retention`, counted back from the newest
command in the index, leave it; the log keeps them. After `history.retention` changes, the index
takes the log again from its start, which leaves it as a fresh index under the new retention would
be; until that pass passes its old newest command, the startup check stays on that command. The node
reads its log once at startup, and before recovery replays that read and writes anything, the index
checks that it holds the newest command the index has taken, unchanged, and resumes after the last
command it took. If the log ends before that newest command or holds another one there, the log was
rolled back, truncated, or replaced, and the node refuses to start (`history_recovery_failed`) with
the log as it found it. The check compares that one command, so a log forged to end with it passes;
a host able to forge the log can rewrite the index as well. A power loss can also leave the index
ahead of the log, since neither syncs every write. SQLite opens the file in defensive mode, with
triggers and views off, its schema untrusted, and cell sizes checked, and every write and read
refuses, inside its own transaction, a schema this code did not create. Removing `history.dir` while
the node is stopped rebuilds the index from the log's segments. Commands logged before events were
recorded add nothing to history.

## Configuration

`CONFIG_PATH`: path to the node config file, used when `run` is given no config flag.

## Testing

```bash
cargo test -p engine-node

# The trusted time dev scenario, offline against loopback Roughtime servers
cargo test -p engine-node trusted_time::tests::scenario
```

## Authors

The monkeytrading contributors

## License

Licensed under the Apache License, Version 2.0; see `LICENSE`.
