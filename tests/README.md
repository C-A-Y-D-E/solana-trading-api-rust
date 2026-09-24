# Tests

```text
tests/unit/                       Private-module unit tests, grouped by owner
tests/unit/client/live.rs          Ignored read-only Jupiter RPC check
tests/unit/dexes/                  Venue fixtures and ignored RPC checks/simulations
tests/unit/jupiter.rs              Offline HTTP fixtures (localhost only)
tests/unit/dflow.rs                Offline DFlow HTTP fixtures and routing checks
tests/unit/dflow/fallback.rs       SOL aggregator comparison, native fallback and fee checks
tests/unit/dflow/sponsorship.rs    Sponsor-aware provider requests and USDC route/fee checks
tests/unit/gas_sponsor.rs          Automatic reimbursement reserve, validation and fee ATAs
tests/unit/gas_sponsor/native.rs   Native setup, WSOL deposit ownership and IDL checks
tests/unit/gas_sponsor/cost.rs     Cost-plus-service math, price/cost caps and simulation metadata
tests/unit/executor/sponsorship.rs Two-signer transaction checks and failure handling
tests/unit/dflow/live.rs           Ignored read-only DFlow preparation and ALT loading
tests/unit/examples/               Ignored, read-only ALT example simulation
tests/simulation/swaps.rs          Public-API simulations; no signing or broadcast
tests/simulation/sponsored.rs      Zero-SOL native USDC simulations including cost recovery
tests/live/buy_sell.rs             Real-money round trip; ignored by default
```

Cargo target names are unchanged: `swap_simulation` and `live_buy`. Files under
`tests/unit/` are loaded through `#[cfg(test)]` and `#[path]` in their owning
modules, so they retain private access without exposing SDK internals. Library
unit tests still run with `cargo test --lib`; the ALT test runs with
`cargo test --example create_shared_alt` and remains ignored by default.
New standalone test targets must be registered with `[[test]]` in `Cargo.toml`;
automatic integration-test discovery is disabled to keep the categories explicit.

## Default checks (no real funds)

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Dependencies must be downloaded on the first run; add `--offline` afterward if
needed. Jupiter/DFlow fixture tests bind a localhost port, so they need loopback access,
but they never call either provider or mainnet. All external-network and real-money tests
are ignored by default. Never run a blanket `cargo test -- --ignored`.

Sponsorship tests use localhost API fixtures and mocked RPC simulations. They
check buy/sell fee budgets, sponsor-paid ATA creation, provider parameters,
signature slots, signatures over the same message, size limits including the
extra signature, and stopping on signing/simulation failures. Cost-recovery tests
also cover the 1 USDC service fee, costs above 1 USDC, stale/invalid prices, SOL/USDC
caps, same-simulation balance metadata, final re-simulation and fee finalization
before both signers. A constructor-only sponsor is checked end-to-end to ensure
cost + 1 USDC billing is automatic without any optional configuration. Native builder fixtures cover Pump.fun/PumpSwap buy/sell setup,
separate account payers and reclaiming only sponsor-funded WSOL rent. Any signatures use
fresh ephemeral keypairs; no wallet files are read. These are not live evidence
that a particular DEX route succeeds with a zero-SOL user. Use the optional
sponsored simulations below for that scenario.

The current repository ignore rule `/tests` excludes new test files from normal
`git add`. Include these files explicitly or adjust that rule before publishing a
checkout expected to run the suite; the `#[path]` modules require them.

## DFlow API smoke check (no real funds)

This optional check prepares SOL↔USDC instructions and fetches their ALTs from
public mainnet endpoints. It uses a public example address, never loads private
keys, and does not simulate, sign, or broadcast. Endpoint/rate-limit failures can
make this check fail independently of the offline suite.

```sh
cargo test --locked --lib dflow::tests::live::prepare_sol_usdc_routes -- --exact --ignored --nocapture
```

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

cargo test --test swap_simulation simulate_usdc_buy -- --ignored --exact --nocapture
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
positive minimum output, 1% fee calculation (sell fees use quoted expected
output), and the fee recipient's simulated SOL/USDC increase equals exactly one
fee. Use a quiet fee wallet because before/after snapshots can otherwise include
unrelated transfers. A new USDC recipient ATA is created in simulation when needed;
the existing SOL-funded recipient avoids rent failures for tiny SOL fees.

Repeat with both SOL-quoted and USDC-quoted PumpSwap pools, and a live Pump.fun
curve, to cover direct and bridged routes. One pool does not cover every route.

### Native sponsorship with a zero-SOL user (simulation only)

Use the same public configuration above, but `SIM_WALLET` must have **zero SOL**
and the input tokens (at least 10 USDC for buys). `SIM_SPONSOR` must be a different
public wallet with more than 0.02 SOL. No keypairs are loaded or signatures produced.
The test's submitter calls only `simulateTransaction`, never `sendTransaction`.
Jupiter/DFlow are disabled so successful execution proves the selected native path.

```sh
export SIM_SPONSOR='<SOL-funded sponsor PUBLIC address>'
export SIM_USDC_UNITS_PER_SOL='<USDC-per-SOL price times 1000000, integer>'
cargo test --test swap_simulation sponsored::simulate_zero_sol_usdc_buy -- --ignored --exact --nocapture
cargo test --test swap_simulation sponsored::simulate_zero_sol_usdc_sell -- --ignored --exact --nocapture
```

The manually supplied price is test input, not a live oracle. The test uses a
0.02 SOL simulated expense cap, 5 USDC total charge ceiling, 1 USDC service fee,
and an additional 1% SDK trading fee. Buys reserve the whole ceiling, leaving any
unused reserve in the wallet. Sells need enough token balance and output to cover
both fees. Simulated buys do not fund subsequent sells. These tests require RPC
simulation balance metadata; unavailable metadata and account/size errors fail
explicitly. They are ignored by default and must be run against appropriate public
addresses before claiming live zero-SOL compatibility for a particular pool.
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
PUMP balance, including pre-existing holdings. Both sides prefer PumpSwap
pool `2uF4Xh61rDwxnG9woyxsVQP7zuA6kLFpb3NvnRQeoiSd` (PUMP/USDC), with SOL
settlement bridged through USDC. The client loads shared ALT
`DG8Y7fV6NaiFBu1LfjNquqbqcPFAVvFA8uP1DhCVC5vb` for both transactions.
It charges a 1% SDK fee in SOL on both sides: buy fees use gross input; sell fees
use quoted expected proceeds, not minimum or actual proceeds. The 0.001 SOL buy budget
includes its 0.00001 SOL application fee. The recipient is required and must differ
from the trading wallet; use an existing SOL-funded wallet to avoid rent failures
on tiny transfers to a new account. Because this pool is USDC-quoted, the client
compares the native route with Jupiter before execution, even though settlement
is SOL. The highest minimum output after the SDK fee wins, excluding gas/rent.
DFlow joins comparisons only when the caller configures `with_dflow`; this live
fixture does not configure it. Aggregator errors do not block a usable native
candidate. Direct native SOL-pair trades use aggregators only after native
quoting, pool lookup, or instruction preparation fails or times out. Simulation,
signing, submission and confirmation errors stop execution without another route
attempt; no successful swap is guaranteed.
Keep extra SOL for network fees and rent.

The non-ignored tests in this target only check path and fee configuration; they never load
your real keypair or access RPC:

```sh
cargo test --test live_buy
```
