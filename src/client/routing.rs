use std::future::Future;
use std::time::Duration;

use super::*;

const ROUTE_PREPARATION_TIMEOUT: Duration = Duration::from_secs(15);

impl TradingClient {
    pub(super) async fn should_compare_routes(&self, trade: &Trade) -> Result<bool> {
        if trade.venue.is_none()
            || trade.settlement == Settlement::Usdc
            || trade.mint == crate::USDC_MINT
        {
            return Ok(true);
        }
        match trade.venue {
            Some(Venue::PumpSwap) if trade.pool.is_some() => {
                bounded_route("pumpswap", async {
                    self.pumpswap
                        .quote_mint(trade)
                        .await
                        .map(|mint| mint == crate::USDC_MINT)
                        .map_err(|error| dex_err("pumpswap", error))
                })
                .await
            }
            // A selected native venue stays native-first for SOL pairs.
            _ => Ok(false),
        }
    }

    pub(super) async fn prepare_best_swap(&self, trade: &Trade) -> Result<PreparedSwap> {
        let adjusted = self.route_trade(trade)?;
        let (native, aggregators) = tokio::join!(
            self.prepare_native_candidate(&adjusted),
            self.prepare_aggregator_candidates(&adjusted),
        );
        select_best_swap(
            trade,
            self.sdk_fee,
            self.sponsor_for(trade),
            native.into_iter().chain(aggregators),
        )
    }

    pub(super) async fn prepare_aggregator_fallback(
        &self,
        trade: &Trade,
        native_error: TradeError,
    ) -> Result<PreparedSwap> {
        let adjusted = self.route_trade(trade)?;
        let aggregators = self.prepare_aggregator_candidates(&adjusted).await;
        // Preserve the native failure if no provider works, without retrying the native route.
        select_best_swap(
            trade,
            self.sdk_fee,
            self.sponsor_for(trade),
            std::iter::once(Err(native_error)).chain(aggregators),
        )
    }

    async fn prepare_native_candidate(&self, trade: &Trade) -> Option<Result<PreparedSwap>> {
        trade.venue?;
        let native = self.venue_adapter(trade.venue);
        Some(
            bounded_route(native.name(), async {
                let prepared = if trade.settlement == Settlement::Usdc {
                    usdc::prepare(trade, &self.pumpfun, &self.pumpswap).await
                } else {
                    native.prepare_swap(trade).await
                }
                .map_err(|error| dex_err(native.name(), error))?;
                match self.sponsor_for(trade) {
                    Some(sponsor) => crate::gas_sponsor::native::fund_native_setup(
                        &self.rpc,
                        prepared,
                        trade.wallet,
                        sponsor.wallet(),
                    )
                    .await
                    .map_err(|error| dex_err(native.name(), error)),
                    None => Ok(prepared),
                }
            })
            .await,
        )
    }

    async fn prepare_aggregator_candidates(&self, trade: &Trade) -> Vec<Result<PreparedSwap>> {
        let payer = self.sponsor_for(trade).map(GasSponsor::wallet);
        let dflow = async {
            match &self.dflow {
                Some(dflow) => Some(prepare_candidate(dflow, trade, payer.as_ref()).await),
                None => None,
            }
        };
        let (jupiter, dflow) = tokio::join!(
            prepare_candidate(&self.jupiter, trade, payer.as_ref()),
            dflow
        );
        std::iter::once(jupiter).chain(dflow).collect()
    }
}

async fn prepare_candidate(
    venue: &dyn Dex,
    trade: &Trade,
    payer: Option<&Pubkey>,
) -> Result<PreparedSwap> {
    bounded_route(venue.name(), async {
        let result = match payer {
            Some(payer) => venue.prepare_sponsored_swap(trade, payer).await,
            None => venue.prepare_swap(trade).await,
        };
        result.map_err(|error| dex_err(venue.name(), error))
    })
    .await
}

pub(super) async fn bounded_route<T>(
    venue: &'static str,
    preparation: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(ROUTE_PREPARATION_TIMEOUT, preparation)
        .await
        .map_err(|_| TradeError::Venue {
            venue,
            msg: "route preparation timed out".into(),
        })?
}

fn select_best_swap(
    trade: &Trade,
    sdk_fee: Option<SdkFee>,
    sponsor: Option<&GasSponsor>,
    candidates: impl IntoIterator<Item = Result<PreparedSwap>>,
) -> Result<PreparedSwap> {
    let adjusted = trade_after_fee(trade, sdk_fee)?;
    let expected_input = match sponsor {
        Some(sponsor) => sponsor.reserve_fee(&adjusted)?.amount,
        None => adjusted.amount,
    };
    let payer = sponsor.map_or(trade.wallet, GasSponsor::wallet);
    let mut best: Option<PreparedSwap> = None;
    let mut failures = Vec::new();
    // Compares minimum output after SDK fees, excluding gas/rent. Ties prefer native, then Jupiter.
    for candidate in candidates {
        let result = candidate.and_then(|prepared| {
            if prepared.quote.in_amount != expected_input
                || prepared.quote.application_fee != 0
                || prepared.quote.sponsorship_fee != 0
                || prepared.quote.min_out == 0
                || prepared.quote.min_out > prepared.quote.expected_out
                || prepared.instructions.is_empty()
            {
                return Err(TradeError::Build(format!(
                    "{} returned invalid output amounts",
                    prepared.venue
                )));
            }
            let prepared = match sdk_fee {
                Some(fee) => fee.apply_with_payer(trade, prepared, &payer)?,
                None => prepared,
            };
            match sponsor {
                Some(sponsor) => sponsor.apply_to_swap(trade, prepared),
                None => Ok(prepared),
            }
        });
        match result {
            Ok(prepared) => {
                if best
                    .as_ref()
                    .is_none_or(|current| prepared.quote.min_out > current.quote.min_out)
                {
                    best = Some(prepared);
                }
            }
            Err(error) => failures.push(error.to_string()),
        }
    }
    best.ok_or_else(|| TradeError::Build(format!("no usable swap route: {}", failures.join("; "))))
}

#[cfg(test)]
#[path = "../../tests/unit/client/routing.rs"]
mod tests;
