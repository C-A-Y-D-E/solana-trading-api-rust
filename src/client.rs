use std::sync::Arc;
use std::time::Duration;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_message::AddressLookupTableAccount;
use solana_pubkey::Pubkey;
use solana_signature::Signature;

use crate::dexes::common::ata;
use crate::dexes::pumpfun::PumpFun;
use crate::dexes::pumpswap::PumpSwap;
use crate::error::{Result, TradeError};
use crate::executor::{SwapSigners, check_status, confirm, dex_err, output_balance, submit_swap};
use crate::jupiter::Jupiter;
use crate::lookup_table::{load_address_lookup_tables, merge_lookup_tables};
use crate::sdk_fee::SdkFee;
use crate::types::{
    Dex, PreparedSwap, Quote, Settlement, Signer, Submitter, SwapResult, SwapStatus, Trade, Venue,
};
use crate::{DFlow, GasSponsor};

mod routing;
mod usdc;

pub struct TradingClient {
    rpc: Arc<RpcClient>,
    pumpfun: PumpFun,
    pumpswap: PumpSwap,
    jupiter: Jupiter,
    dflow: Option<DFlow>,
    sdk_fee: Option<SdkFee>,
    gas_sponsor: Option<GasSponsor>,
    shared_lookup_tables: Vec<AddressLookupTableAccount>,

    pub deadline: Duration,
}

impl TradingClient {
    pub fn new(
        rpc: Arc<RpcClient>,
        jupiter_base_url: impl Into<String>,
        jupiter_api_key: Option<String>,
    ) -> Self {
        Self {
            pumpfun: PumpFun::new(rpc.clone()),
            pumpswap: PumpSwap::new(rpc.clone()),
            jupiter: Jupiter::new(jupiter_base_url, jupiter_api_key),
            dflow: None,
            sdk_fee: None,
            gas_sponsor: None,
            shared_lookup_tables: Vec::new(),
            deadline: Duration::from_secs(30),
            rpc,
        }
    }

    /// Replaces the default PumpSwap USDC/WSOL bridge pool used by routed buys.
    pub fn with_pumpswap_sol_usdc_pool(mut self, pool: Pubkey) -> Self {
        self.pumpswap = self.pumpswap.with_sol_usdc_pool(pool);
        self
    }

    /// Adds one settlement-currency fee, including aggregator routes.
    pub fn with_sdk_fee(mut self, fee: SdkFee) -> Self {
        self.sdk_fee = Some(fee);
        self
    }

    /// Sponsors every USDC-settled trade on this client, including native routes. SOL trades are unchanged.
    /// Always recovers sponsor expenses plus the configured service fee (1 USDC by default).
    /// This is opt-in policy, not automatic low-balance detection; use a separate unsponsored client otherwise.
    pub fn with_gas_sponsor(mut self, sponsor: GasSponsor) -> Self {
        self.gas_sponsor = Some(sponsor);
        self
    }

    fn sponsor_for(&self, trade: &Trade) -> Option<&GasSponsor> {
        self.gas_sponsor
            .as_ref()
            .filter(|_| trade.settlement == Settlement::Usdc)
    }

    fn route_trade(&self, trade: &Trade) -> Result<Trade> {
        let adjusted = trade_after_fee(trade, self.sdk_fee)?;
        match self.sponsor_for(trade) {
            Some(sponsor) => sponsor.reserve_fee(&adjusted),
            None => Ok(adjusted),
        }
    }

    /// Adds DFlow to aggregator comparisons and native fallbacks. Production requires an API key.
    pub fn with_dflow(mut self, base_url: impl Into<String>, api_key: Option<String>) -> Self {
        self.dflow = Some(DFlow::new(self.rpc.clone(), base_url, api_key));
        self
    }

    fn venue_adapter(&self, venue: Option<Venue>) -> &dyn Dex {
        match venue {
            Some(Venue::PumpFun) => &self.pumpfun,
            Some(Venue::PumpSwap) => &self.pumpswap,
            None => &self.jupiter,
        }
    }

    /// Replaces shared ALT snapshots. Caller-loaded tables must be active on this client's cluster.
    pub fn with_shared_lookup_tables(mut self, tables: Vec<AddressLookupTableAccount>) -> Self {
        self.shared_lookup_tables = merge_lookup_tables(&tables, &[]);
        self
    }

    /// Loads shared ALTs once from this client's RPC; no ALT RPC requests are added to each swap.
    pub async fn with_shared_lookup_table_addresses(
        mut self,
        addresses: &[Pubkey],
    ) -> Result<Self> {
        self.shared_lookup_tables = load_address_lookup_tables(&self.rpc, addresses).await?;
        Ok(self)
    }

    pub fn shared_lookup_tables(&self) -> &[AddressLookupTableAccount] {
        &self.shared_lookup_tables
    }

    /// Reload after extending a table. Failed refreshes leave all existing snapshots unchanged.
    pub async fn refresh_shared_lookup_tables(&mut self) -> Result<()> {
        let addresses: Vec<_> = self
            .shared_lookup_tables
            .iter()
            .map(|table| table.key)
            .collect();
        let refreshed = load_address_lookup_tables(&self.rpc, &addresses).await?;
        self.shared_lookup_tables = refreshed;
        Ok(())
    }

    /// Compares aggregators when no venue is selected, and all available routes for USDC trades.
    /// Selected native SOL venues try aggregators only on preparation failure.
    pub async fn quote(&self, trade: &Trade) -> Result<Quote> {
        let fee_adjusted_trade = self.route_trade(trade)?;
        match self.should_compare_routes(trade).await {
            Ok(true) => return Ok(self.prepare_best_swap(trade).await?.quote),
            Err(error) => return Ok(self.prepare_aggregator_fallback(trade, error).await?.quote),
            Ok(false) => {}
        };
        let primary_venue = self.venue_adapter(trade.venue);
        let venue_quote = match routing::bounded_route(primary_venue.name(), async {
            primary_venue
                .quote(&fee_adjusted_trade)
                .await
                .map_err(|error| dex_err(primary_venue.name(), error))
        })
        .await
        {
            Ok(quote) => quote,
            Err(error) if trade.venue.is_some() => {
                return Ok(self.prepare_aggregator_fallback(trade, error).await?.quote);
            }
            Err(error) => return Err(error),
        };
        match self.sdk_fee {
            Some(fee) => fee.quote_after_fee(trade, venue_quote),
            None => Ok(venue_quote),
        }
    }

    /// Builds the full route and optional fee without signing or sending a transaction.
    /// Native SOL preparation can fall back; simulation, signing and submission never retry a route.
    pub async fn prepare_swap(&self, trade: &Trade) -> Result<PreparedSwap> {
        self.route_trade(trade)?;
        match self.should_compare_routes(trade).await {
            Ok(true) => return self.prepare_best_swap(trade).await,
            Err(error) => return self.prepare_aggregator_fallback(trade, error).await,
            Ok(false) => {}
        }
        match prepare_venue_swap(trade, self.venue_adapter(trade.venue), self.sdk_fee).await {
            Err(error) if trade.venue.is_some() => {
                self.prepare_aggregator_fallback(trade, error).await
            }
            result => result,
        }
    }

    pub async fn swap(
        &self,
        trade: &Trade,
        signer: &dyn Signer,
        submitter: &dyn Submitter,
        priority_fee_lamports: u64,
    ) -> Result<SwapResult> {
        self.swap_with_lookup_tables(trade, signer, submitter, priority_fee_lamports, &[])
            .await
    }

    /// Executes a swap with additional on-chain address lookup tables.
    pub async fn swap_with_lookup_tables(
        &self,
        trade: &Trade,
        signer: &dyn Signer,
        submitter: &dyn Submitter,
        priority_fee_lamports: u64,
        lookup_tables: &[AddressLookupTableAccount],
    ) -> Result<SwapResult> {
        let before = output_balance(&self.rpc, trade).await;

        let pending = self
            .submit_with_lookup_tables(
                trade,
                signer,
                submitter,
                priority_fee_lamports,
                lookup_tables,
            )
            .await?;
        let sig = pending
            .hash
            .parse::<Signature>()
            .map_err(|_| TradeError::Decode("client", format!("bad signature {}", pending.hash)))?;

        let status = confirm(&self.rpc, &sig, self.deadline).await?;
        let amount_received = if status == SwapStatus::Confirmed {
            Some(
                output_balance(&self.rpc, trade)
                    .await
                    .saturating_sub(before),
            )
        } else {
            None
        };
        Ok(SwapResult {
            hash: pending.hash,
            dex: pending.dex,
            status,
            amount_received,
            application_fee: pending.application_fee,
            sponsorship_fee: pending.sponsorship_fee,
        })
    }

    pub async fn submit(
        &self,
        trade: &Trade,
        signer: &dyn Signer,
        submitter: &dyn Submitter,
        priority_fee_lamports: u64,
    ) -> Result<SwapResult> {
        self.submit_with_lookup_tables(trade, signer, submitter, priority_fee_lamports, &[])
            .await
    }

    /// Submits a swap with additional on-chain address lookup tables.
    pub async fn submit_with_lookup_tables(
        &self,
        trade: &Trade,
        signer: &dyn Signer,
        submitter: &dyn Submitter,
        priority_fee_lamports: u64,
        lookup_tables: &[AddressLookupTableAccount],
    ) -> Result<SwapResult> {
        let combined_tables = merge_lookup_tables(lookup_tables, &self.shared_lookup_tables);
        let prepared = self.prepare_swap(trade).await?;
        // No fallback after preparation: a submission error may mean the transaction landed.
        submit_swap(
            &self.rpc,
            prepared,
            SwapSigners {
                user: signer,
                sponsor: self.sponsor_for(trade),
            },
            submitter,
            trade,
            priority_fee_lamports,
            &combined_tables,
        )
        .await
    }

    pub async fn status(&self, hash: &str) -> Result<SwapStatus> {
        let sig = hash
            .parse::<Signature>()
            .map_err(|_| TradeError::Decode("client", format!("bad signature {hash:?}")))?;
        check_status(&self.rpc, &sig).await
    }

    pub async fn token_balance(&self, wallet: &Pubkey, mint: &Pubkey) -> Result<u64> {
        let mint_acc = self
            .rpc
            .get_account(mint)
            .await
            .map_err(|source| TradeError::Rpc {
                context: "get_account(mint)",
                source,
            })?;
        let token_account = ata(wallet, mint, &mint_acc.owner);
        let bal = self
            .rpc
            .get_token_account_balance(&token_account)
            .await
            .map_err(|source| TradeError::Rpc {
                context: "token balance",
                source,
            })?;
        bal.amount
            .parse()
            .map_err(|_| TradeError::Decode("client", format!("bad token amount {}", bal.amount)))
    }
}

pub(crate) async fn prepare_venue_swap(
    trade: &Trade,
    primary_venue: &dyn Dex,
    sdk_fee: Option<SdkFee>,
) -> Result<PreparedSwap> {
    let fee_adjusted_trade = trade_after_fee(trade, sdk_fee)?;
    let prepared = routing::bounded_route(primary_venue.name(), async {
        primary_venue
            .prepare_swap(&fee_adjusted_trade)
            .await
            .map_err(|error| dex_err(primary_venue.name(), error))
    })
    .await?;
    match sdk_fee {
        Some(fee) => fee.apply_to_swap(trade, prepared),
        None => Ok(prepared),
    }
}

fn validate_settlement(trade: &Trade) -> Result<()> {
    if trade.settlement == Settlement::Usdc
        && (trade.amount == 0 || trade.slippage_bps >= 10_000 || trade.mint == crate::USDC_MINT)
    {
        return Err(TradeError::Build("USDC trade requires a different target mint, positive input, and slippage below 10000 bps".into()));
    }
    Ok(())
}

fn trade_after_fee(trade: &Trade, sdk_fee: Option<SdkFee>) -> Result<Trade> {
    validate_settlement(trade)?;
    if trade.amount == 0 || trade.slippage_bps >= 10_000 {
        return Err(TradeError::Build(
            "amount must be positive and slippage below 10000 bps".into(),
        ));
    }
    match sdk_fee {
        Some(fee) => fee.trade_after_fee(trade),
        None => Ok(*trade),
    }
}

#[cfg(test)]
#[path = "../tests/unit/client/mod.rs"]
mod tests;
