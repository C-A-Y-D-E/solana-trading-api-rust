use anyhow::{Result, ensure};

use crate::dexes::common::WSOL;
use crate::lookup_table::merge_lookup_tables;
use crate::{
    Dex, PreparedSwap, Pubkey, PumpFun, PumpSwap, Quote, Settlement, Side, Trade, USDC_MINT, Venue,
};

#[cfg(test)]
#[path = "../../tests/unit/client/usdc.rs"]
mod tests;

pub(super) async fn prepare(
    trade: &Trade,
    pumpfun: &PumpFun,
    pumpswap: &PumpSwap,
) -> Result<PreparedSwap> {
    let target: &dyn Dex = match trade.venue {
        Some(Venue::PumpFun) => pumpfun,
        Some(Venue::PumpSwap) => {
            match pumpswap.quote_mint(trade).await? {
                USDC_MINT => return pumpswap.prepare_swap(trade).await,
                WSOL => {}
                _ => anyhow::bail!("unsupported PumpSwap quote currency"),
            }
            pumpswap
        }
        None => {
            anyhow::bail!("native USDC routing requires PumpFun or PumpSwap")
        }
    };
    prepare_via_sol(trade, target, pumpswap, pumpswap.sol_usdc_pool()).await
}

async fn prepare_via_sol(
    trade: &Trade,
    target: &dyn Dex,
    bridge: &dyn Dex,
    bridge_pool: Pubkey,
) -> Result<PreparedSwap> {
    let (bridge_slippage, target_slippage) = PumpSwap::route_slippage(trade.slippage_bps);
    let mut target_trade = Trade {
        settlement: Settlement::Sol,
        slippage_bps: target_slippage,
        ..*trade
    };
    let mut bridge_trade = Trade {
        settlement: Settlement::Sol,
        mint: USDC_MINT,
        venue: Some(Venue::PumpSwap),
        pool: Some(bridge_pool),
        slippage_bps: bridge_slippage,
        side: if trade.side == Side::Buy {
            Side::Sell
        } else {
            Side::Buy
        },
        ..*trade
    };
    // Fixed second-leg input uses only guaranteed SOL; favorable excess stays with the user.
    let (mut first, second, bridge_fee) = match trade.side {
        Side::Buy => {
            let first = bridge.prepare_swap(&bridge_trade).await?;
            ensure!(
                first.quote.min_out > 0,
                "USDC bridge minimum output is zero"
            );
            target_trade.amount = first.quote.min_out;
            let second = target.prepare_swap(&target_trade).await?;
            let fee = first.quote.fee;
            (first, second, fee)
        }
        Side::Sell => {
            let first = target.prepare_swap(&target_trade).await?;
            ensure!(first.quote.min_out > 0, "target minimum SOL output is zero");
            bridge_trade.amount = first.quote.min_out;
            let second = bridge.prepare_swap(&bridge_trade).await?;
            let fee = second.quote.fee;
            (first, second, fee)
        }
    };
    ensure!(
        second.quote.min_out > 0,
        "USDC route minimum output is zero"
    );
    first.instructions.extend(second.instructions);
    Ok(PreparedSwap {
        venue: target.name(),
        quote: Quote {
            in_amount: trade.amount,
            price_impact_bps: crate::price_impact::combine(
                first.quote.price_impact_bps,
                second.quote.price_impact_bps,
            ),
            fee: bridge_fee,
            ..second.quote
        },
        instructions: first.instructions,
        lookup_tables: merge_lookup_tables(&first.lookup_tables, &second.lookup_tables),
    })
}
