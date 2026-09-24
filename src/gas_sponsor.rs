use std::sync::Arc;

use crate::error::{Result, TradeError};
use crate::sdk_fee::append_usdc_fee;
use crate::{PreparedSwap, Pubkey, Settlement, Side, Signer, Trade};

mod cost;
pub(crate) mod native;
use cost::SponsorCostPolicy;
pub use cost::{SolUsdcPrice, SolUsdcPriceSource};

const DEFAULT_MAX_COST_LAMPORTS: u64 = 10_000_000;
const DEFAULT_MAX_FEE_USDC: u64 = 3_000_000;

/// Opt-in USDC sponsorship. Keep the signer on your backend, never in an untrusted client.
pub struct GasSponsor {
    wallet: Pubkey,
    pub(crate) signer: Arc<dyn Signer>,
    fee_recipient: Pubkey,
    pub(crate) cost_policy: SponsorCostPolicy,
}

impl GasSponsor {
    /// Always recovers simulated costs plus 1 USDC, paid to the sponsor wallet by default.
    /// Default ceilings: 0.01 SOL net expense and 3 USDC total charge. Keep the price feed trusted.
    pub fn new(
        wallet: Pubkey,
        signer: Arc<dyn Signer>,
        price_source: Arc<dyn SolUsdcPriceSource>,
    ) -> Result<Self> {
        if wallet == Pubkey::default() {
            return Err(TradeError::Build("sponsor wallet must be nonzero".into()));
        }
        Ok(Self {
            wallet,
            signer,
            fee_recipient: wallet,
            cost_policy: SponsorCostPolicy::new(
                price_source,
                DEFAULT_MAX_COST_LAMPORTS,
                DEFAULT_MAX_FEE_USDC,
            )?,
        })
    }

    pub fn with_fee_recipient(mut self, recipient: Pubkey) -> Result<Self> {
        if recipient == Pubkey::default() {
            return Err(TradeError::Build(
                "sponsorship fee recipient must be nonzero".into(),
            ));
        }
        self.fee_recipient = recipient;
        Ok(self)
    }

    pub fn with_limits(mut self, max_cost_lamports: u64, max_fee_usdc: u64) -> Result<Self> {
        self.cost_policy = self
            .cost_policy
            .with_limits(max_cost_lamports, max_fee_usdc)?;
        Ok(self)
    }

    /// Changes only the service fee; sponsor expenses are still recovered, even when this is zero.
    pub fn with_service_fee_usdc(mut self, amount: u64) -> Result<Self> {
        self.cost_policy = self.cost_policy.with_service_fee_usdc(amount)?;
        Ok(self)
    }

    pub fn wallet(&self) -> Pubkey {
        self.wallet
    }

    pub(crate) fn reserve_fee(&self, trade: &Trade) -> Result<Trade> {
        if trade.settlement != Settlement::Usdc || trade.wallet == self.wallet {
            return Err(TradeError::Build(
                "sponsorship requires USDC settlement and a separate fee payer".into(),
            ));
        }
        let mut adjusted = *trade;
        if self.fee_recipient == trade.wallet {
            return Err(TradeError::Build(
                "sponsorship fee recipient must differ from trading wallet".into(),
            ));
        }
        if trade.side == Side::Buy {
            // Reserve the ceiling; submit/swap lower the charge before signing. Unused USDC stays with the user.
            adjusted.amount = subtract_fee(trade.amount, self.cost_policy.max_fee_usdc)?;
        }
        Ok(adjusted)
    }

    pub(crate) fn apply_to_swap(
        &self,
        trade: &Trade,
        mut prepared: PreparedSwap,
    ) -> Result<PreparedSwap> {
        prepared.quote.in_amount = trade.amount;
        let ceiling = self.cost_policy.max_fee_usdc;
        if trade.side == Side::Sell {
            prepared.quote.min_out = subtract_fee(prepared.quote.min_out, ceiling)?;
            prepared.quote.expected_out = subtract_fee(prepared.quote.expected_out, ceiling)?;
        }
        prepared.quote.sponsorship_fee = ceiling;
        // Atomic with the swap: failure refunds USDC, but the sponsor still loses network fees.
        append_usdc_fee(
            &mut prepared.instructions,
            &self.wallet,
            &trade.wallet,
            &self.fee_recipient,
            ceiling,
        )?;
        Ok(prepared)
    }
}

fn subtract_fee(amount: u64, fee: u64) -> Result<u64> {
    amount
        .checked_sub(fee)
        .filter(|remaining| *remaining > 0)
        .ok_or_else(|| {
            TradeError::Build(
                "USDC amount must exceed the combined trading and sponsorship fees".into(),
            )
        })
}

#[cfg(test)]
#[path = "../tests/unit/gas_sponsor.rs"]
pub(crate) mod tests;
