# Solana Trading SDK

Private Rust SDK for Pump.fun, PumpSwap, and Jupiter swaps. Callers provide their
own signer and transaction submitter. The SDK supports SOL/USDC settlement,
optional application fees, and shared address lookup tables. No custom router
deployment is required for SDK fees.

## Layout

```text
src/
  client.rs, client/        Public trading workflow and USDC routing
  dexes/                   Pump.fun/PumpSwap adapters, IDLs and venue tests
  jupiter.rs, jupiter/     Jupiter adapter and offline HTTP fixture tests
  sdk_fee.rs, sdk_fee/     Settlement-currency fees and tests
  executor.rs, executor/   Transaction preparation/submission and tests
  lookup_table.rs, .../    Shared ALT loading and tests
  price_impact.rs, .../    Curve-impact calculation and tests
  types.rs, error.rs       Public contracts and errors
  submit.rs               RPC and bloXroute submitters
tests/
  simulation/             Unsigned, read-only mainnet simulations
  live/                   Explicitly ignored tests that spend real funds
  README.md               Test setup, commands and safety limitations
examples/
  create_shared_alt.rs    ALT creation utility (running it spends SOL)
docs/                     SDK usage and architecture references
```

Unit tests live beside their owning modules. Ignored network checks are separated
into `tests/live.rs` within those modules where private helpers are required.
The archived `programs/` directory is local-only, ignored by Git, and is not part
of this crate's build. Cargo keeps `publish = false`; pushing to GitHub does not
publish the package to crates.io.

## Check the project

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Default tests do not sign, broadcast, or call mainnet. Some offline tests use a
localhost HTTP server. See [test instructions](tests/README.md) before enabling
any ignored test; `live_buy` spends funds and has a documented stale-balance
limitation. Do not use a blanket `--ignored` command.

## Use the SDK

See the [usage guide](docs/usage.mdx) for client setup, SOL/USDC settlement,
application fees, Jupiter routing, ALTs, and signing. The [DEX layer guide](docs/dex-layer.mdx)
describes the adapter architecture.

SDK fees are optional and bypassable. Buy fees use gross input; sell fees use
guaranteed minimum output, not actual proceeds. A successful simulation is not a
guarantee that a future transaction will land.

## Before pushing

Keep keypairs outside the repository, preferably in your existing Solana config
directory. `.env`, `.env.*`, project `config/`, `*-keypair.json`, `.pem`, `.key`,
build output, and `.DS_Store` are ignored. Ignore rules do not remove secrets from
past commits or already tracked files.

Review the exact changes before staging and pushing:

```sh
git status --short
git diff --check
git diff
# After selecting files to stage:
git diff --cached --stat
git diff --cached
```

Do not include wallet JSON, seed phrases, private keys, API keys, or credentialed
RPC URLs. The tests use public fixture addresses; they do not require private
keys unless an explicitly ignored live-trading test is run.
