# No unapproved deploys

Load on request before any command that changes a live environment.

**Rule**: Never deploy or otherwise mutate a live environment without approval for that action.

- Covers:
  - deploying or restarting an enclave image, or changing its config;
  - sequencing any `Admin` command into a running engine (`SetParams`, `UpsertMarket`,
    `SetBenchmarkRate`, `SetStressFloor`, `SetReduceOnly`), which changes the protocol rules for
    every account;
  - `HouseCapital` moves;
  - deploying, upgrading, or calling a contract, including posting checkpoints and submitting
    withdrawals on-chain;
  - writing secrets or keys, and handing over enclave keys between operators.
- A local dev node on `config/dev.toml` with mock custody is not a live environment; anything
  holding real funds, or connected to a public chain, is one.
- Approval covers one action. A yes to deploy is not a yes to change its config, and a yes on
  testnet is not a yes on mainnet.
- When in doubt, propose the exact command and wait.
