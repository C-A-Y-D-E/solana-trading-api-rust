# Tests

```text
src/<module>/tests.rs              Offline unit tests beside the implementation
src/<module>/tests/live.rs         Ignored read-only RPC checks
src/jupiter/tests.rs               Offline HTTP fixtures (localhost only)
src/dexes/pumpswap/tests/          USDC fixtures and ignored route simulations
tests/simulation/swaps.rs          Public-API simulations; no signing or broadcast
tests/live/buy_sell.rs             Real-money round trip; ignored by default
```

Cargo target names are unchanged: `swap_simulation` and `live_buy`. Tests in
`src/` retain access to private implementation details without exposing them in
the public SDK. `examples/create_shared_alt.rs` also contains an ignored,
read-only ALT simulation.
New standalone test targets must be registered with `[[test]]` in `Cargo.toml`;
automatic integration-test discovery is disabled to keep the categories explicit.

## Default checks (no real funds)

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Dependencies must be downloaded on the first run; add `--offline` afterward if
needed. Jupiter fixture tests bind a localhost port, so they need loopback access,
but they never call Jupiter or mainnet. All external-network and real-money tests
are ignored by default. Never run a blanket `cargo test -- --ignored`.

## Swap simulations

`swap_simulation` prepares real Pump.fun/PumpSwap routes with a 1% SDK fee, compiles
unsigned v0 transactions using on-chain ALTs, and calls `simulateTransaction`.
It never loads a private key, signs, or broadcasts. The four live tests are ignored
by default; the harness checks also run offline.

```sh
cargo test --test swap_simulation
```

Set public addresses for the mainnet scenario you want to test:

```sh
export SIM_WALLET='<funded trading wallet public key>'
export SIM_FEE_WALLET='<different, quiet, existing SOL-funded wallet public key>'
export SIM_MINT='<target token mint>'
export SIM_VENUE='pumpswap' # or pumpfun for a live, unmigrated curve
export SIM_POOL='<target PumpSwap pool>' # omit for automatic discovery / Pump.fun
export SIM_ALTS='DG8Y7fV6NaiFBu1LfjNquqbqcPFAVvFA8uP1DhCVC5vb' # comma-separated; optional
export SIM_SELL_AMOUNT='<token base units already held by SIM_WALLET>'
# Optional: export SIM_RPC_URL='<your mainnet RPC URL>'

cargo test --test swap_simulation -- --ignored --nocapture --test-threads=1
```

To run only one scenario:

```sh
cargo test --test swap_simulation simulate_sol_buy -- --ignored --exact --nocapture
```

Buy inputs are 0.001 SOL and 1 USDC. Sells use the configured token base units.
The trading wallet must already hold the input plus SOL for transaction fees and
account rent. Each simulation is independent: a simulated buy does **not** fund
the sell tests. Missing configuration/balances and RPC failures fail the test;
they are not silently skipped. Do not use a private key as any address.

Checks: successful on-chain simulation, transaction size at most 1,232 bytes,
positive minimum output, 1% fee calculation (sell fees use guaranteed minimum
output), and the fee recipient's simulated SOL/USDC increase equals exactly one
fee. Use a quiet fee wallet because before/after snapshots can otherwise include
unrelated transfers. A new USDC recipient ATA is created in simulation when needed;
the existing SOL-funded recipient avoids rent failures for tiny SOL fees.

Repeat with both SOL-quoted and USDC-quoted PumpSwap pools, and a live Pump.fun
curve, to cover direct and bridged routes. One pool does not cover every route.
The harness uses the maximum compute budget and no submitter tip; it does not test
the executor's adaptive budget, signing, broadcast, or confirmation. Passing a
simulation is evidence for that route and current state, not a landing guarantee.

Do not run all ignored tests indiscriminately: `live_buy` is a separate test target
that **does send real transactions**. Use the exact `--test swap_simulation` target.

## Live swaps with the config wallet

`live_buy` now reads the Solana CLI keypair JSON at
`~/.config/solana/fee-router-admin.json` by default, not `PRIVATE_KEY` from `.env`.
The existing wallet remains there; no key file is copied into this repository.
Override with `LIVE_WALLET_PATH=./config/wallet.json` for a project-local wallet,
or an absolute path. The entire project `config/` directory is Git-ignored.

**Known limitation:** this live test waits for confirmed transactions but reads
balances at the RPC's default finalized commitment. It can therefore sell a stale,
pre-buy balance and leave newly bought PUMP behind. It also does not assert final
transaction statuses or a zero post-sell token balance. This organization change
does not fix that behavior; do not treat a passing test as proof of full liquidation.

Only run the following when you intend to spend real mainnet funds:

```sh
export LIVE_FEE_WALLET='<your fee recipient public address, different from the trading wallet>'
cargo test --test live_buy buy_then_sell_all -- --ignored --exact --nocapture
```

This test buys 0.001 SOL of PUMP with 10% slippage, then sells the **whole**
PUMP balance, including pre-existing holdings. Both sides explicitly use PumpSwap
pool `2uF4Xh61rDwxnG9woyxsVQP7zuA6kLFpb3NvnRQeoiSd` (PUMP/USDC), with SOL
settlement bridged through USDC. The client loads shared ALT
`DG8Y7fV6NaiFBu1LfjNquqbqcPFAVvFA8uP1DhCVC5vb` for both transactions.
It charges a 1% SDK fee in SOL on both sides: buy fees use gross input; sell fees
use guaranteed minimum proceeds, not actual proceeds. The 0.001 SOL buy budget
includes its 0.00001 SOL application fee. The recipient is required and must differ
from the trading wallet; use an existing SOL-funded wallet to avoid rent failures
on tiny transfers to a new account. Fee-enabled routing never falls back to
Jupiter: a pool/preparation error fails with the original venue error.
Keep extra SOL for network fees and rent.

The non-ignored tests in this target only check path and fee configuration; they never load
your real keypair or access RPC:

```sh
cargo test --test live_buy
```
