use super::*;
use crate::dexes::common::{TOKEN_PROGRAM, system_transfer};
use crate::{AddressLookupTableAccount, SdkFee};
use std::sync::Mutex;

struct VenueFixture {
    calls: Mutex<Vec<Trade>>,
    minimum: u64,
    price_impact_bps: Option<f64>,
    table: Pubkey,
}

impl VenueFixture {
    fn new(minimum: u64) -> Self {
        Self {
            calls: Mutex::new(vec![]),
            minimum,
            price_impact_bps: Some(1_000.0),
            table: Pubkey::new_unique(),
        }
    }
}

#[async_trait::async_trait]
impl Dex for VenueFixture {
    fn name(&self) -> &'static str {
        "fixture"
    }
    async fn quote(&self, _: &Trade) -> Result<Quote> {
        panic!("route must use prepared minimum")
    }
    async fn prepare_swap(&self, trade: &Trade) -> Result<PreparedSwap> {
        self.calls.lock().unwrap().push(*trade);
        Ok(PreparedSwap {
            venue: self.name(),
            quote: Quote {
                in_amount: trade.amount,
                price_impact_bps: self.price_impact_bps,
                expected_out: self.minimum + 100,
                min_out: self.minimum,
                fee: 1,
                application_fee: 0,
            },
            instructions: vec![system_transfer(&trade.wallet, &self.table, trade.amount)],
            lookup_tables: vec![AddressLookupTableAccount {
                key: self.table,
                addresses: vec![self.table],
            }],
        })
    }
}

fn trade(side: Side, venue: Venue) -> Trade {
    Trade {
        side,
        ..Trade::buy(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            1_000_000,
            300,
            Some(venue),
        )
        .with_settlement(Settlement::Usdc)
    }
}

#[tokio::test]
async fn usdc_buys_bridge_net_usdc_and_spend_only_guaranteed_sol() {
    for venue in [Venue::PumpFun, Venue::PumpSwap] {
        let target = VenueFixture::new(50_000);
        let bridge = VenueFixture::new(90_000);
        let pool = Pubkey::new_unique();
        let trade = trade(Side::Buy, venue);
        let fee = SdkFee::new(Pubkey::new_unique(), 100).unwrap();
        let adjusted = fee.venue_trade(&trade).unwrap();
        let prepared = prepare_via_sol(&adjusted, &target, &bridge, pool)
            .await
            .unwrap();
        let prepared = fee.apply(&trade, prepared).unwrap();
        let bridge_call = bridge.calls.lock().unwrap()[0];
        assert_eq!(
            (bridge_call.side, bridge_call.mint, bridge_call.pool),
            (Side::Sell, USDC_MINT, Some(pool))
        );
        assert_eq!(bridge_call.amount, 990_000);
        let target_call = target.calls.lock().unwrap()[0];
        assert_eq!(
            (target_call.amount, target_call.settlement),
            (90_000, Settlement::Sol)
        );
        assert_eq!(
            (bridge_call.slippage_bps, target_call.slippage_bps),
            (50, 250)
        );
        assert_eq!(
            (
                prepared.quote.in_amount,
                prepared.quote.min_out,
                prepared.quote.application_fee
            ),
            (1_000_000, 50_000, 10_000)
        );
        assert_eq!(prepared.instructions.len(), 4);
        assert_eq!(prepared.quote.price_impact_bps, Some(1_900.0));
        assert_eq!(
            prepared.instructions.last().unwrap().program_id,
            TOKEN_PROGRAM
        );
        assert_eq!(prepared.lookup_tables.len(), 2);
    }
}

#[tokio::test]
async fn usdc_sells_bridge_guaranteed_sol_and_charge_final_usdc_once() {
    for venue in [Venue::PumpFun, Venue::PumpSwap] {
        let target = VenueFixture::new(50_000);
        let bridge = VenueFixture::new(90_000);
        let trade = trade(Side::Sell, venue);
        let prepared = prepare_via_sol(&trade, &target, &bridge, Pubkey::new_unique())
            .await
            .unwrap();
        let fee = SdkFee::new(Pubkey::new_unique(), 100).unwrap();
        let prepared = fee.apply(&trade, prepared).unwrap();
        let bridge_call = bridge.calls.lock().unwrap()[0];
        assert_eq!((bridge_call.side, bridge_call.amount), (Side::Buy, 50_000));
        assert_eq!(target.calls.lock().unwrap()[0].amount, trade.amount);
        assert_eq!(
            (
                prepared.quote.expected_out,
                prepared.quote.min_out,
                prepared.quote.application_fee
            ),
            (89_200, 89_100, 900)
        );
        let transfer = prepared.instructions.last().unwrap();
        assert_eq!(prepared.quote.price_impact_bps, Some(1_900.0));
        assert_eq!(&transfer.data[1..9], &900_u64.to_le_bytes());
        assert_eq!(transfer.accounts[1].pubkey, USDC_MINT);
    }
}

#[tokio::test]
async fn zero_intermediate_or_final_output_rejects_the_route() {
    for side in [Side::Buy, Side::Sell] {
        for (target_min, bridge_min) in [(0, 100), (100, 0)] {
            let result = prepare_via_sol(
                &trade(side, Venue::PumpFun),
                &VenueFixture::new(target_min),
                &VenueFixture::new(bridge_min),
                Pubkey::new_unique(),
            )
            .await;
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn unknown_impact_on_either_hop_makes_route_impact_unknown() {
    for side in [Side::Buy, Side::Sell] {
        for (target_impact, bridge_impact) in [(None, Some(100.0)), (Some(100.0), None)] {
            let target = VenueFixture {
                price_impact_bps: target_impact,
                ..VenueFixture::new(50_000)
            };
            let bridge = VenueFixture {
                price_impact_bps: bridge_impact,
                ..VenueFixture::new(90_000)
            };
            let prepared = prepare_via_sol(
                &trade(side, Venue::PumpFun),
                &target,
                &bridge,
                Pubkey::new_unique(),
            )
            .await
            .unwrap();
            assert_eq!(prepared.quote.price_impact_bps, None);
        }
    }
}
