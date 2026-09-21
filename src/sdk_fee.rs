use solana_pubkey::Pubkey;

use crate::USDC_MINT;
use crate::dexes::common::{TOKEN_PROGRAM, ata, create_ata_idempotent, system_transfer};
use crate::error::{Result, TradeError};
use crate::types::{PreparedSwap, Quote, Settlement, Side, Trade};

const BASIS_POINTS: u64 = 10_000;
const USDC_DECIMALS: u8 = 6;

#[cfg(test)]
mod tests;

/// Optional, bypassable SDK fee in the trade's settlement currency; no custom program required.
#[derive(Clone, Copy, Debug)]
pub struct SdkFee {
    recipient: Pubkey,
    basis_points: u16,
}

impl SdkFee {
    /// 100 basis points is 1%. Zero disables charging; 100% and above are rejected.
    pub fn new(recipient: Pubkey, basis_points: u16) -> Result<Self> {
        if recipient == Pubkey::default() || u64::from(basis_points) >= BASIS_POINTS {
            return Err(TradeError::Build(
                "SDK fee requires a nonzero recipient and fee below 10000 bps".into(),
            ));
        }
        Ok(Self {
            recipient,
            basis_points,
        })
    }

    pub fn recipient(&self) -> Pubkey {
        self.recipient
    }

    pub fn basis_points(&self) -> u16 {
        self.basis_points
    }

    fn amount(&self, basis: u64) -> u64 {
        (u128::from(basis) * u128::from(self.basis_points) / u128::from(BASIS_POINTS)) as u64
    }

    pub(crate) fn venue_trade(&self, trade: &Trade) -> Result<Trade> {
        if trade.amount == 0 || trade.slippage_bps >= BASIS_POINTS {
            return Err(TradeError::Build(
                "amount must be positive and slippage below 10000 bps".into(),
            ));
        }
        if self.basis_points > 0 && trade.wallet == self.recipient {
            return Err(TradeError::Build(
                "SDK fee recipient must differ from trading wallet".into(),
            ));
        }
        let mut adjusted = *trade;
        if trade.side == Side::Buy {
            adjusted.amount -= self.amount(trade.amount);
        }
        Ok(adjusted)
    }

    pub(crate) fn net_quote(&self, trade: &Trade, mut quote: Quote) -> Result<Quote> {
        if quote.min_out == 0 || quote.expected_out < quote.min_out {
            return Err(TradeError::Build(
                "SDK fee route requires a positive, valid minimum output".into(),
            ));
        }
        quote.in_amount = trade.amount;
        // A fixed transfer cannot calculate a percentage of actual sell proceeds on-chain.
        quote.application_fee = self.amount(match trade.side {
            Side::Buy => trade.amount,
            Side::Sell => quote.min_out,
        });
        if trade.side == Side::Sell {
            quote.expected_out -= quote.application_fee;
            quote.min_out -= quote.application_fee;
        }
        Ok(quote)
    }

    pub(crate) fn apply(&self, trade: &Trade, mut prepared: PreparedSwap) -> Result<PreparedSwap> {
        prepared.quote = self.net_quote(trade, prepared.quote)?;
        if prepared.quote.application_fee > 0 {
            // Append after the full route, never per hop; a failed transfer rolls back the swap.
            match trade.settlement {
                Settlement::Sol => prepared.instructions.push(system_transfer(
                    &trade.wallet,
                    &self.recipient,
                    prepared.quote.application_fee,
                )),
                Settlement::Usdc => {
                    prepared.instructions.push(create_ata_idempotent(
                        &trade.wallet,
                        &self.recipient,
                        &USDC_MINT,
                        &TOKEN_PROGRAM,
                    ));
                    prepared.instructions.push(
                        spl_token::instruction::transfer_checked(
                            &TOKEN_PROGRAM,
                            &ata(&trade.wallet, &USDC_MINT, &TOKEN_PROGRAM),
                            &USDC_MINT,
                            &ata(&self.recipient, &USDC_MINT, &TOKEN_PROGRAM),
                            &trade.wallet,
                            &[],
                            prepared.quote.application_fee,
                            USDC_DECIMALS,
                        )
                        .map_err(|error| {
                            TradeError::Build(format!("USDC fee transfer: {error}"))
                        })?,
                    );
                }
            }
        }
        Ok(prepared)
    }
}
