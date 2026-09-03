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
use crate::types::{Dex, Quote, Signer, Submitter, SwapResult, SwapStatus, Trade, Venue};

pub struct TradingClient {
    rpc: Arc<RpcClient>,
    pumpfun: PumpFun,
    pumpswap: PumpSwap,
    jupiter: Jupiter,

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
            deadline: Duration::from_secs(30),
            rpc,
        }
    }

    /// Replaces the default PumpSwap USDC/WSOL bridge pool used by routed buys.
    pub fn with_pumpswap_sol_usdc_pool(mut self, pool: Pubkey) -> Self {
        self.pumpswap = self.pumpswap.with_sol_usdc_pool(pool);
        self
    }

    pub async fn quote(&self, t: &Trade) -> Result<Quote> {
        let primary = match t.venue {
            Some(Venue::PumpFun) => venue_err("pumpfun", self.pumpfun.quote(t).await),
            Some(Venue::PumpSwap) => venue_err("pumpswap", self.pumpswap.quote(t).await),
            None => venue_err("jupiter", self.jupiter.quote(t).await),
        };
        match primary {
            Ok(q) => Ok(q),
            Err(e) if allows_jupiter_fallback(t.venue) => {
                eprintln!(
                    "trading-client: {:?} quote failed ({e}); falling back to Jupiter",
                    t.venue
                );
                venue_err("jupiter", self.jupiter.quote(t).await)
            }
            Err(e) => Err(e),
        }
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
        let dex: &dyn Dex = match t.venue {
            Some(Venue::PumpFun) => &self.pumpfun,
            Some(Venue::PumpSwap) => &self.pumpswap,
            None => &self.jupiter,
        };
        match submit_swap(
            &self.rpc,
            dex,
            signer,
            submitter,
            t,
            priority_fee_lamports,
            lookup_tables,
        )
        .await
        {
            Ok(r) => Ok(r),
            Err(e) if allows_jupiter_fallback(t.venue) => {
                eprintln!(
                    "trading-client: {:?} pre-send failed ({e}); falling back to Jupiter",
                    t.venue
                );
                submit_swap(
                    &self.rpc,
                    &self.jupiter,
                    signer,
                    submitter,
                    t,
                    priority_fee_lamports,
                    lookup_tables,
                )
                .await
            }
            Err(e) => Err(e),
        }
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

fn allows_jupiter_fallback(venue: Option<Venue>) -> bool {
    matches!(venue, Some(Venue::PumpFun))
}

fn venue_err(venue: &'static str, r: anyhow::Result<Quote>) -> Result<Quote> {
    r.map_err(|e| dex_err(venue, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_pubkey::pubkey;

    fn jupiter_api_key() -> Option<String> {
        let env = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok()?;
        env.lines().find_map(|l| {
            l.trim()
                .strip_prefix("JUPITER_API_KEY=")
                .map(|v| v.trim().trim_matches('"').to_string())
        })
    }

    #[test]
    fn pumpswap_never_falls_back_to_jupiter() {
        assert!(!allows_jupiter_fallback(Some(Venue::PumpSwap)));
        assert!(allows_jupiter_fallback(Some(Venue::PumpFun)));
    }

    #[tokio::test]
    #[ignore = "live: needs JUPITER_API_KEY in .env"]
    async fn jupiter_quote() {
        let Some(key) = jupiter_api_key() else {
            println!("skip: no JUPITER_API_KEY in .env");
            return;
        };
        let rpc = Arc::new(RpcClient::new(
            "https://api.mainnet-beta.solana.com".to_string(),
        ));
        let client = TradingClient::new(rpc, "https://api.jup.ag", Some(key));

        let trade = Trade::buy(
            pubkey!("11111111111111111111111111111111"),
            pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            1_000_000,
            100,
            None,
        );
        let q = client.quote(&trade).await.expect("jupiter quote failed");
        println!(
            "jupiter buy 0.001 SOL → USDC: expect {} (min {})",
            q.expected_out, q.min_out
        );
        assert!(q.expected_out > 0);
    }
}
