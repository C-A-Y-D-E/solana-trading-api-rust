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
use crate::executor::{check_status, confirm, dex_err, output_balance, submit_swap};
use crate::jupiter::Jupiter;
use crate::lookup_table::{load_address_lookup_tables, merge_lookup_tables};
use crate::sdk_fee::SdkFee;
use crate::types::{
    Dex, PreparedSwap, Quote, Settlement, Signer, Submitter, SwapResult, SwapStatus, Trade, Venue,
};

mod usdc;

pub struct TradingClient {
    rpc: Arc<RpcClient>,
    pumpfun: PumpFun,
    pumpswap: PumpSwap,
    jupiter: Jupiter,
    sdk_fee: Option<SdkFee>,
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
            sdk_fee: None,
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

    /// Adds a settlement-currency fee, including explicit Jupiter routes (venue None).
    /// Automatic Jupiter fallback remains disabled when a fee is configured.
    pub fn with_sdk_fee(mut self, fee: SdkFee) -> Self {
        self.sdk_fee = Some(fee);
        self
    }

    fn dex(&self, venue: Option<Venue>) -> &dyn Dex {
        match venue {
            Some(Venue::PumpFun) => &self.pumpfun,
            Some(Venue::PumpSwap) => &self.pumpswap,
            None => &self.jupiter,
        }
    }

    fn fallback(&self, venue: Option<Venue>) -> Option<&dyn Dex> {
        (self.sdk_fee.is_none() && allows_jupiter_fallback(venue))
            .then_some(&self.jupiter as &dyn Dex)
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

    pub async fn quote(&self, t: &Trade) -> Result<Quote> {
        if t.settlement == Settlement::Usdc {
            return Ok(self.prepare_swap(t).await?.quote);
        }
        validate_settlement(t)?;
        let adjusted = self.sdk_fee.map_or(Ok(*t), |fee| fee.venue_trade(t))?;
        let dex = self.dex(t.venue);
        let quote = match (dex.quote(&adjusted).await, self.fallback(t.venue)) {
            (Ok(quote), _) => quote,
            (Err(_), Some(fallback)) => {
                venue_err(fallback.name(), fallback.quote(&adjusted).await)?
            }
            (Err(error), None) => return Err(dex_err(dex.name(), error)),
        };
        self.sdk_fee
            .map_or(Ok(quote), |fee| fee.net_quote(t, quote))
    }

    /// Builds the full route and optional fee without signing or sending a transaction.
    pub async fn prepare_swap(&self, trade: &Trade) -> Result<PreparedSwap> {
        if trade.settlement == Settlement::Usdc && trade.venue.is_some() {
            validate_settlement(trade)?;
            let adjusted = self
                .sdk_fee
                .map_or(Ok(*trade), |fee| fee.venue_trade(trade))?;
            let prepared = usdc::prepare(&adjusted, &self.pumpfun, &self.pumpswap)
                .await
                .map_err(|error| dex_err("usdc-route", error))?;
            return match self.sdk_fee {
                Some(fee) => fee.apply(trade, prepared),
                None => Ok(prepared),
            };
        }
        prepare_trade(
            trade,
            self.dex(trade.venue),
            self.fallback(trade.venue),
            self.sdk_fee,
        )
        .await
    }

    pub async fn swap(
        &self,
        t: &Trade,
        signer: &dyn Signer,
        submitter: &dyn Submitter,
        priority_fee_lamports: u64,
    ) -> Result<SwapResult> {
        self.swap_with_lookup_tables(t, signer, submitter, priority_fee_lamports, &[])
            .await
    }

    /// Executes a swap with additional on-chain address lookup tables.
    pub async fn swap_with_lookup_tables(
        &self,
        t: &Trade,
        signer: &dyn Signer,
        submitter: &dyn Submitter,
        priority_fee_lamports: u64,
        lookup_tables: &[AddressLookupTableAccount],
    ) -> Result<SwapResult> {
        let before = output_balance(&self.rpc, t).await;

        let pending = self
            .submit_with_lookup_tables(t, signer, submitter, priority_fee_lamports, lookup_tables)
            .await?;
        let sig = pending
            .hash
            .parse::<Signature>()
            .map_err(|_| TradeError::Decode("client", format!("bad signature {}", pending.hash)))?;

        let status = confirm(&self.rpc, &sig, self.deadline).await?;
        let amount_received = if status == SwapStatus::Confirmed {
            Some(output_balance(&self.rpc, t).await.saturating_sub(before))
        } else {
            None
        };
        Ok(SwapResult {
            hash: pending.hash,
            dex: pending.dex,
            status,
            amount_received,
        })
    }

    pub async fn submit(
        &self,
        t: &Trade,
        signer: &dyn Signer,
        submitter: &dyn Submitter,
        priority_fee_lamports: u64,
    ) -> Result<SwapResult> {
        self.submit_with_lookup_tables(t, signer, submitter, priority_fee_lamports, &[])
            .await
    }

    /// Submits a swap with additional on-chain address lookup tables.
    pub async fn submit_with_lookup_tables(
        &self,
        t: &Trade,
        signer: &dyn Signer,
        submitter: &dyn Submitter,
        priority_fee_lamports: u64,
        lookup_tables: &[AddressLookupTableAccount],
    ) -> Result<SwapResult> {
        let combined_tables = merge_lookup_tables(lookup_tables, &self.shared_lookup_tables);
        let prepared = self.prepare_swap(t).await?;
        // No fallback after preparation: a submission error may mean the transaction landed.
        submit_swap(
            &self.rpc,
            prepared,
            signer,
            submitter,
            t,
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

pub(crate) async fn prepare_trade(
    trade: &Trade,
    primary: &dyn Dex,
    fallback: Option<&dyn Dex>,
    fee: Option<SdkFee>,
) -> Result<PreparedSwap> {
    validate_settlement(trade)?;
    let adjusted = fee.map_or(Ok(*trade), |fee| fee.venue_trade(trade))?;
    let prepared = match (primary.prepare_swap(&adjusted).await, fallback) {
        (Ok(prepared), _) => prepared,
        (Err(_), Some(fallback)) if fee.is_none() && trade.settlement == Settlement::Sol => {
            fallback
                .prepare_swap(&adjusted)
                .await
                .map_err(|error| dex_err(fallback.name(), error))?
        }
        (Err(error), _) => return Err(dex_err(primary.name(), error)),
    };
    match fee {
        Some(fee) => fee.apply(trade, prepared),
        None => Ok(prepared),
    }
}

fn allows_jupiter_fallback(venue: Option<Venue>) -> bool {
    matches!(venue, Some(Venue::PumpFun))
}

fn validate_settlement(trade: &Trade) -> Result<()> {
    if trade.settlement == Settlement::Usdc
        && (trade.amount == 0 || trade.slippage_bps >= 10_000 || trade.mint == crate::USDC_MINT)
    {
        return Err(TradeError::Build("USDC trade requires a different target mint, positive input, and slippage below 10000 bps".into()));
    }
    Ok(())
}

fn venue_err(venue: &'static str, r: anyhow::Result<Quote>) -> Result<Quote> {
    r.map_err(|e| dex_err(venue, e))
}

#[cfg(test)]
mod tests;
