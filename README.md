# Solana Trading SDK

Private Rust SDK for Pump.fun, PumpSwap, Jupiter, and DFlow swaps. Callers provide their
own signer and transaction submitter. The SDK supports SOL/USDC settlement,
optional application fees, and shared address lookup tables. No custom router
deployment is required for SDK fees.

## Layout

```text
src/
  client.rs, client/        Public trading workflow and USDC routing
  dexes/                   Pump.fun/PumpSwap adapters and IDLs
  jupiter.rs              Jupiter adapter
  dflow.rs                DFlow atomic-swap adapter
  sdk_fee.rs              Settlement-currency fees
  gas_sponsor.rs          Separate fee payer and USDC reimbursement policy
  gas_sponsor/native.rs   Sponsor-funded native account setup and WSOL rent recovery
  gas_sponsor/cost.rs     Capped simulation-based cost plus service fee
  executor.rs             Transaction preparation/submission
  lookup_table.rs         Shared ALT loading
  price_impact.rs         Curve-impact calculation
  types.rs, error.rs       Public contracts and errors
  submit.rs               RPC and bloXroute submitters
tests/
  unit/                   Module tests, HTTP fixtures and ignored RPC checks
  simulation/             Unsigned, read-only mainnet simulations
  live/                   Explicitly ignored tests that spend real funds
  README.md               Test setup, commands and safety limitations
examples/
  create_shared_alt.rs    ALT creation utility (running it spends SOL)
docs/                     SDK usage and architecture references
```

All SDK and example test code lives under `tests/`. Unit test files are attached
to their owning modules with `#[cfg(test)]` and `#[path]`, preserving private access
without adding public SDK APIs. Their ignored RPC checks remain in separate files.
The archived `programs/` directory is local-only, ignored by Git, and is not part
of this crate's build. Cargo keeps `publish = false`; pushing to GitHub does not
publish the package to crates.io.

## Check the project

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Default tests never use your wallets, broadcast, or call mainnet. Signing tests use
ephemeral test keypairs; some offline tests use a localhost HTTP server. See [test instructions](tests/README.md) before enabling
any ignored test; `live_buy` spends funds and has a documented stale-balance
limitation. Do not use a blanket `--ignored` command.

## Use the SDK

See the [usage guide](docs/usage.mdx) for client setup, SOL/USDC settlement,
application fees, Jupiter routing, ALTs, and signing. The [DEX layer guide](docs/dex-layer.mdx)
describes the adapter architecture.

SDK fees are optional and bypassable. Buy fees use gross input; sell fees use
quoted expected output, not minimum or actual proceeds. The sell fee is fixed
when preparing the swap and is subtracted from both expected and minimum output.
Quotes are rejected if the fees would leave no positive minimum output. A successful
simulation is not a guarantee that a future transaction will land.

Direct SOL-pair trades try Pump.fun/PumpSwap first. If native quoting, pool lookup,
or instruction preparation fails or times out, Jupiter and configured DFlow compete
as fallbacks. A successful native route makes no aggregator requests.
USDC settlement, SOL↔USDC trades, and SOL-settled trades through a USDC-quoted
PumpSwap pool automatically compare the native route, Jupiter, and configured
DFlow. Supply the USDC pool with `with_pool`; the SDK does not search for pools.
An unreadable supplied pool triggers aggregator fallback, without retrying the
native lookup. `venue: None` compares Jupiter and configured DFlow for both SOL
and USDC settlement, without native pool lookups. Without DFlow configured, only
Jupiter participates in this aggregator-only comparison.

Candidates use the same input budget and requested slippage. Highest minimum
output after the SDK fee wins; ties prefer native, then Jupiter. Gas, ATA rent,
and leftover intermediate tokens are not included in this comparison. No DEX
allowlist is sent to either aggregator. Failed, malformed, or timed-out candidates
are skipped if another usable route exists; preparation is capped at 15 seconds
per candidate/lookup. Invalid input or SDK fee configuration fails before routing,
without fallback requests. Only the winner is simulated and signed, so simulation, signing,
submission, or confirmation failure stops the trade without another route attempt.
`prepare_swap(...).venue` identifies the winner. A later `swap` prepares fresh
routes and may choose differently. Aggregator price impact is unavailable (`None`)
because provider figures are not the SDK's curve-only metric.

Add DFlow once on the client:

```rust,ignore
let client = TradingClient::new(rpc, jupiter_url, jupiter_api_key)
    .with_dflow("https://quote-api.dflow.net", Some(dflow_api_key))
    .with_sdk_fee(SdkFee::new(fee_wallet, 100)?);
```

Production requires an `x-api-key`; the public development endpoint is
`https://dev-quote-api.dflow.net` with `None` for the key (still mainnet assets,
not Solana devnet). See [DFlow authentication](https://pond.dflow.net/resources/recipes/api-keys).
The adapter uses `/quote` → `/swap-instructions` for one atomic v0 transaction,
loads provider ALTs, and leaves priority fees and simulation to the executor.
It does not use intent orders or provider platform fees;
the existing SDK fee is applied once to the selected trade.

## Sponsored USDC trades (user needs no SOL)

Configure the sponsor once. It handles account funding, network fees, and automatic
USDC reimbursement of simulated net SOL expense plus a **1 USDC service fee**:

```rust,ignore
use solana_trading_api::{GasSponsor, SdkFee, Settlement, Trade, TradingClient};

// sponsor_signer: Arc<dyn solana_trading_api::Signer>, backed by your wallet/KMS.
// price_source: Arc<dyn SolUsdcPriceSource>, your trusted backend SOL/USDC feed.
let sponsor = GasSponsor::new(sponsor_wallet, sponsor_signer, price_source)?;

let client = TradingClient::new(rpc, jupiter_url, jupiter_api_key)
    .with_dflow("https://quote-api.dflow.net", Some(dflow_api_key))
    .with_sdk_fee(SdkFee::new(fee_wallet, 100)?) // Optional additional 1% trading fee.
    .with_gas_sponsor(sponsor);

let trade = Trade::buy(user_wallet, token_mint, 10_000_000, 100, None)
    .with_settlement(Settlement::Usdc);
let quote = client.quote(&trade).await?;
// 10 USDC budget: 0.10 trading fee + 3 sponsorship reserve + 6.90 swapped.
assert_eq!(quote.sponsorship_fee, 3_000_000);

let result = client.swap(&trade, user_signer, submitter, 5_000).await?;
// result.sponsorship_fee is the final simulated-cost + service charge encoded in the transaction.
```

The sponsor wallet receives reimbursement by default. There is no separate billing
opt-in, fixed-fee mode or free-sponsorship mode. Default limits are **0.01 SOL** net
sponsor expense and **3 USDC** total charge. Optional settings stay on the sponsor:

```rust,ignore
let sponsor = sponsor
    .with_fee_recipient(fee_wallet)?
    .with_limits(20_000_000, 5_000_000)? // 0.02 SOL expense / 5 USDC total charge.
    .with_service_fee_usdc(1_000_000)?;  // Default: 1 USDC; zero still recovers expenses.
```

The sponsor pays SOL network fees, account rent (including fee-recipient ATAs),
priority fees, and submitter tips. User and sponsor sign the same final transaction;
the user remains the token owner. Existing signer adapters work for both roles.
Keep the sponsor funded with SOL. No contract deployment or user SOL top-up is needed.

This client sponsors **every USDC-settled buy/sell**, regardless of the user's SOL
balance. It is opt-in, not automatic balance detection. Use an unsponsored client
for ordinary USDC trades. SOL-settled trades on either client remain unsponsored.
Sponsored routing includes the selected Pump.fun/PumpSwap venue alongside
sponsor-aware Jupiter/DFlow builds. Native preparation funds missing token accounts,
volume accumulators and creator-vault rent from the sponsor before the swap. It
preserves the user as trader/token owner. Sponsor-funded temporary WSOL deposits
are returned to the sponsor after closing; pre-existing user deposits are not.
Account setup races can fail preparation/execution and require a fresh swap.
No venue selected still means aggregator-only comparison. If no sponsored route
works, return an error; never downgrade to an unsponsored route. See the official
[Jupiter payer](https://developers.jup.ag/docs/swap/advanced/gasless) and
[DFlow sponsorship](https://pond.dflow.net/spot/trading/sponsored-swaps) contracts.

The sponsorship charge is **additional** to `with_sdk_fee`; omit `with_sdk_fee`
to charge only sponsor expenses plus the service fee. Buys reserve both
fees from the gross input; sells subtract them from guaranteed USDC proceeds.
Amounts must remain positive after fees. `Quote.application_fee` and
`Quote.sponsorship_fee` itemize the charges; outputs are already net of fees.
Selling does not require an existing USDC balance. A sell percentage is based on
gross quoted expected output, before the sponsorship ceiling; the minimum
must cover both fees and leave positive proceeds.

The USDC charge transfers atomically with the swap. Failed on-chain execution
reverts swaps and USDC fees, **but the sponsor still pays network fees**.
USDC is not a guaranteed dollar.
Enforce authentication, rate limits, and spending/priority-fee caps in your backend;
users can close sponsored token accounts and make you fund their rent again.
Never expose the sponsor key or offer an unrestricted transaction-signing endpoint.
Build transactions server-side with trusted providers and authorize the requested
trade before allowing the sponsor signer to sign. No economic-abuse protection is
implemented by this SDK configuration.

`prepare_swap` remains unsigned and includes the fee ceiling. Use `submit`/`swap`
to finalize reimbursement and collect both signatures. Shared ALTs still apply, and the packet-size check counts
both signatures. Quote/preparation may choose a different route on a later swap.

### How automatic sponsor reimbursement works

Every sponsor charges simulation-estimated net SOL spending
(network/priority fees, tips, and account deposits minus SOL returned to the sponsor)
converted to USDC, plus a default **1 USDC service fee**. This policy applies to
native, Jupiter and DFlow routes. It does not charge swap principal or add another
percentage trading fee. `GasSponsor::new` requires the price source up front so a
sponsor cannot be configured without the conversion needed for reimbursement.

`SolUsdcPriceSource::sol_usdc_price()` returns `SolUsdcPrice` with
`usdc_units_per_sol` (six-decimal USDC units; 150 USDC/SOL = 150_000_000) and
`observed_at` (the feed's `SystemTime` observation timestamp). There is no built-in
oracle or hardcoded market price. Zero, future-dated or older-than-30-second rates
are rejected. The source has a five-second timeout. Keep it backend-controlled;
do not accept a user's rate. `sponsor.with_service_fee_usdc(amount)` changes only
the service fee. Even with zero service fee, expenses are still recovered.

`quote` / `prepare_swap` reserve the **full configured USDC ceiling**, not an exact
cost estimate. On buys, that ceiling and the SDK fee are removed from the gross
input before routing; unused reserved USDC remains in the wallet, not swapped.
Consequently the input must cover the ceiling even if the eventual charge is lower.
On sells, quoted outputs conservatively subtract the ceiling. Route comparison uses
these conservative outputs, not final route-specific gas costs.

`submit` / `swap` simulate the built transaction, read its sponsor pre/post SOL
balances from the same simulation, replace only the sponsorship transfer amount,
then simulate again before either signer is called. The priority fee, tips, rent
for both fee-recipient accounts and the second signature are included. The RPC must
return simulation `fee`, `preBalances` and `postBalances`; missing data, changed
cost, simulation errors, stale prices or either exceeded cap stop before signing.
No route is retried after these execution checks. `SwapResult.sponsorship_fee`
reports the amount encoded in the submitted transaction, collected only on success.

Example: a simulated 0.08 USDC expense results in a 1.08 USDC charge, even if the
quote reserved 3 USDC. A 1.20 USDC expense results in 2.20 USDC, subject to the caps.
Manual submission of `prepare_swap` instructions would charge the ceiling;
**use `submit` / `swap` for cost recovery**.

This is pre-signing, simulation-based billing, not exact post-execution accounting
or an on-chain spending cap. Account state and SOL/USDC prices can move before the
transaction lands. Persistent account deposits remain an expense to the sponsor,
even when the user may later close an account and reclaim its rent. On failure the
USDC transfer rolls back but network fees remain the sponsor's loss. No live
zero-SOL execution guarantee is implied by the offline tests.

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
