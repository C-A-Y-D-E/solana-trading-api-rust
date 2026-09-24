use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_message::VersionedMessage;

use crate::error::{Result, TradeError};
use crate::{RpcClient, VersionedTransaction};

const LAMPORTS_PER_SOL: u128 = 1_000_000_000;
const ONE_USDC: u64 = 1_000_000;
const MAX_PRICE_AGE: Duration = Duration::from_secs(30);
const PRICE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub struct SolUsdcPrice {
    pub usdc_units_per_sol: u64,
    pub observed_at: SystemTime,
}

/// Supply a trusted backend price feed, not a price submitted by the trading user.
#[async_trait]
pub trait SolUsdcPriceSource: Send + Sync {
    async fn sol_usdc_price(&self) -> anyhow::Result<SolUsdcPrice>;
}

pub(crate) struct SponsorCostPolicy {
    price_source: Arc<dyn SolUsdcPriceSource>,
    max_cost_lamports: u64,
    pub(super) max_fee_usdc: u64,
    service_fee_usdc: u64,
}

impl SponsorCostPolicy {
    /// Caps simulated net sponsor spending and total USDC reimbursement; adds 1 USDC by default.
    pub fn new(
        price_source: Arc<dyn SolUsdcPriceSource>,
        max_cost_lamports: u64,
        max_fee_usdc: u64,
    ) -> Result<Self> {
        if max_cost_lamports == 0 || max_fee_usdc <= ONE_USDC {
            return Err(build_error(
                "cost cap must be positive and USDC cap must exceed 1 USDC",
            ));
        }
        Ok(Self {
            price_source,
            max_cost_lamports,
            max_fee_usdc,
            service_fee_usdc: ONE_USDC,
        })
    }

    pub fn with_service_fee_usdc(mut self, amount: u64) -> Result<Self> {
        if amount >= self.max_fee_usdc {
            return Err(build_error("service fee must be below the USDC cap"));
        }
        self.service_fee_usdc = amount;
        Ok(self)
    }

    pub(super) fn with_limits(mut self, max_cost_lamports: u64, max_fee_usdc: u64) -> Result<Self> {
        if max_cost_lamports == 0 || max_fee_usdc <= self.service_fee_usdc {
            return Err(build_error(
                "cost cap must be positive and USDC cap must exceed the service fee",
            ));
        }
        self.max_cost_lamports = max_cost_lamports;
        self.max_fee_usdc = max_fee_usdc;
        Ok(self)
    }

    pub(crate) async fn finalize_fee(
        &self,
        rpc: &RpcClient,
        transaction: &mut VersionedTransaction,
        transfer_index: usize,
    ) -> Result<u64> {
        let cost = simulated_cost(rpc, transaction).await?;
        let price = tokio::time::timeout(PRICE_TIMEOUT, self.price_source.sol_usdc_price())
            .await
            .map_err(|_| build_error("SOL/USDC price request timed out"))?
            .map_err(|error| build_error(&format!("SOL/USDC price: {error:#}")))?;
        let charge = self.charge(cost, price, SystemTime::now())?;
        replace_reimbursement(transaction, transfer_index, self.max_fee_usdc, charge)?;
        // Account state can change between simulations and execution. This is a bounded estimate,
        // not receipt-based billing or an on-chain sponsor spending limit.
        if simulated_cost(rpc, transaction).await? != cost {
            return Err(build_error(
                "sponsor cost changed during preparation; request a fresh swap",
            ));
        }
        self.charge(cost, price, SystemTime::now())?;
        Ok(charge)
    }

    fn charge(&self, cost: u64, price: SolUsdcPrice, now: SystemTime) -> Result<u64> {
        let age = now
            .duration_since(price.observed_at)
            .map_err(|_| build_error("SOL/USDC price timestamp is in the future"))?;
        if price.usdc_units_per_sol == 0 || age > MAX_PRICE_AGE {
            return Err(build_error(
                "SOL/USDC price must be positive and no older than 30 seconds",
            ));
        }
        if cost > self.max_cost_lamports {
            return Err(build_error(
                "simulated sponsor SOL cost exceeds the configured cap",
            ));
        }
        let reimbursement =
            (u128::from(cost) * u128::from(price.usdc_units_per_sol)).div_ceil(LAMPORTS_PER_SOL);
        let charge = reimbursement + u128::from(self.service_fee_usdc);
        if charge > u128::from(self.max_fee_usdc) {
            return Err(build_error(
                "sponsor reimbursement exceeds the configured USDC cap",
            ));
        }
        Ok(charge as u64)
    }
}

async fn simulated_cost(rpc: &RpcClient, transaction: &VersionedTransaction) -> Result<u64> {
    let simulation = rpc
        .simulate_transaction_with_config(
            transaction,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: false,
                ..Default::default()
            },
        )
        .await
        .map_err(|source| TradeError::Rpc {
            context: "simulate sponsor cost",
            source,
        })?
        .value;
    if let Some(error) = simulation.err {
        return Err(TradeError::Simulation {
            error: format!("{error:?}"),
            logs: simulation.logs.unwrap_or_default(),
        });
    }
    // Same-simulation balances avoid attributing concurrent sponsor transactions to this user.
    let before = simulation
        .pre_balances
        .as_ref()
        .and_then(|balances| balances.first());
    let after = simulation
        .post_balances
        .as_ref()
        .and_then(|balances| balances.first());
    let fee = simulation
        .fee
        .ok_or_else(|| build_error("RPC omitted simulated network fee"))?;
    let cost = before.zip(after).and_then(|(before, after)| before.checked_sub(*after))
        .ok_or_else(|| build_error("RPC omitted valid pre/post sponsor balances; cost recovery requires simulation balance metadata"))?;
    if cost < fee {
        return Err(build_error(
            "simulated sponsor debit is below the network fee",
        ));
    }
    Ok(cost)
}

fn replace_reimbursement(
    transaction: &mut VersionedTransaction,
    index: usize,
    reserved: u64,
    charge: u64,
) -> Result<()> {
    let VersionedMessage::V0(message) = &mut transaction.message else {
        return Err(build_error(
            "sponsor cost recovery requires a v0 transaction",
        ));
    };
    let instruction = message
        .instructions
        .get_mut(index)
        .ok_or_else(|| build_error("missing sponsorship transfer"))?;
    let mut expected = vec![12];
    expected.extend_from_slice(&reserved.to_le_bytes());
    expected.push(6);
    if instruction.data != expected || charge > reserved {
        return Err(build_error("unexpected sponsorship transfer"));
    }
    instruction.data[1..9].copy_from_slice(&charge.to_le_bytes());
    Ok(())
}

fn build_error(message: &str) -> TradeError {
    TradeError::Build(message.into())
}

#[cfg(test)]
#[path = "../../tests/unit/gas_sponsor/cost.rs"]
mod tests;
