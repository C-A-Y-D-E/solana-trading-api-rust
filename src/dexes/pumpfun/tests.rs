mod live;

use super::*;
use crate::types::Venue;

#[test]
fn derives_user_volume_accumulator_from_trading_wallet() {
    let wallet: Pubkey = "BwuECfotadkbcPqcjjFJfY4khc1MHtLiC3B4gMW1gx5z"
        .parse()
        .unwrap();

    assert_eq!(
        PumpFun::user_volume_pda(&wallet).to_string(),
        "2RaTH6dUbkGL5trjw4JrAPBNR6iAuMCmzBqf3TPibYUr"
    );
}

#[test]
fn selects_reserved_fee_recipient_for_mayhem_curve() {
    let fees = FeeSettings {
        standard_recipient: PROGRAM_ID,
        mayhem_recipient: FEE_PROGRAM_ID,
        buyback_recipient: Pubkey::default(),
        protocol_bps: 0,
        creator_bps: 0,
    };

    assert_eq!(fees.recipient(false), PROGRAM_ID);
    assert_eq!(fees.recipient(true), FEE_PROGRAM_ID);
}

#[test]
fn price_impact_uses_virtual_reserves_and_excludes_fees_and_slippage() {
    let dex = PumpFun::new(Arc::new(RpcClient::new_mock("fails".into())));
    let mut curve = Curve {
        creator: Pubkey::default(),
        base_reserves: 1_000_000,
        quote_reserves: 2_000_000,
        real_token_reserves: 1_000_000,
        is_mayhem: false,
        is_cashback: false,
        token_program: TOKEN_PROGRAM,
    };
    let fees = FeeSettings {
        standard_recipient: Pubkey::default(),
        mayhem_recipient: Pubkey::default(),
        buyback_recipient: Pubkey::default(),
        protocol_bps: 100,
        creator_bps: 0,
    };
    for side in [Side::Buy, Side::Sell] {
        let quote = dex.compute_quote(&curve, &fees, side, 100_000, 100);
        let wider = dex.compute_quote(&curve, &fees, side, 100_000, 900);
        assert_eq!(quote.price_impact_bps, wider.price_impact_bps);
        let (reserve, input) = match side {
            Side::Buy => (curve.quote_reserves, 100_000 - quote.fee),
            Side::Sell => (curve.base_reserves, 100_000),
        };
        let expected = 10_000.0 * input as f64 / (reserve as f64 + input as f64);
        assert!((quote.price_impact_bps.unwrap() - expected).abs() < 1e-9);
    }
    curve.real_token_reserves = 1;
    assert_eq!(
        dex.compute_quote(&curve, &fees, Side::Buy, 100_000, 100)
            .price_impact_bps,
        None
    );
}
