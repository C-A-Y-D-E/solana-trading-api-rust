mod live;
mod sell_simulation;
mod usdc;

use super::*;
use crate::types::Venue;
use solana_message::AddressLookupTableAccount;

#[test]
fn decodes_legacy_and_virtual_reserve_pool_layouts() {
    let mut data = vec![0; 245];
    assert_eq!(decode_pool(&data).unwrap().1, 0);
    data.extend_from_slice(&500_000_i128.to_le_bytes());
    assert_eq!(decode_pool(&data).unwrap().1, 500_000);
    assert!(decode_pool(&data[..250]).is_err());
}

#[test]
fn effective_reserves_include_signed_virtual_liquidity() {
    assert_eq!(effective_quote_reserves(100, 900).unwrap(), 1_000);
    assert_eq!(effective_quote_reserves(100, -50).unwrap(), 50);
    assert!(effective_quote_reserves(100, -101).is_err());
    assert!(effective_quote_reserves(u64::MAX, 1).is_err());
    assert!(effective_quote_reserves(1, i128::MAX).is_err());
}

fn test_fees() -> FeeSettings {
    FeeSettings {
        standard_protocol_recipient: Pubkey::new_unique(),
        mayhem_protocol_recipient: Pubkey::new_unique(),
        buyback_recipient: Pubkey::new_unique(),
        lp_bps: 20,
        protocol_bps: 5,
        creator_bps: 0,
    }
}

fn test_pool(base_mint: Pubkey, quote_mint: Pubkey) -> PoolState {
    PoolState {
        coin_creator: Pubkey::default(),
        base_mint,
        quote_mint,
        base_vault: Pubkey::new_unique(),
        quote_vault: Pubkey::new_unique(),
        base_reserves: 1_000_000_000_000,
        quote_reserves: 10_000_000_000_000,
        is_mayhem: false,
        is_cashback: false,
        base_token_program: TOKEN_PROGRAM,
        quote_token_program: TOKEN_PROGRAM,
    }
}

#[test]
fn selects_reserved_protocol_recipient_for_mayhem_pool() {
    let fees = FeeSettings {
        standard_protocol_recipient: PROGRAM_ID,
        mayhem_protocol_recipient: FEE_PROGRAM_ID,
        buyback_recipient: Pubkey::default(),
        lp_bps: 0,
        protocol_bps: 0,
        creator_bps: 0,
    };

    assert_eq!(fees.protocol_recipient(false), PROGRAM_ID);
    assert_eq!(fees.protocol_recipient(true), FEE_PROGRAM_ID);
}

#[test]
fn route_slippage_reserves_at_most_fifty_bps_for_the_bridge() {
    assert_eq!(PumpSwap::route_slippage(30), (15, 15));
    assert_eq!(PumpSwap::route_slippage(500), (50, 450));
}

#[test]
fn direct_price_impact_uses_reserves_and_net_input_not_slippage_or_output_fees() {
    let dex = PumpSwap::new(Arc::new(RpcClient::new_mock("fails".into())));
    let pool = test_pool(Pubkey::new_unique(), WSOL);
    let fees = test_fees();
    let amount = 1_000_000_000_000;
    for side in [Side::Buy, Side::Sell] {
        let quote = dex.compute_quote(&pool, &fees, side, amount, 100);
        let wider = dex.compute_quote(&pool, &fees, side, amount, 900);
        assert_eq!(quote.price_impact_bps, wider.price_impact_bps);
        assert_ne!(quote.min_out, wider.min_out);
        let (input_reserve, net_input) = match side {
            Side::Buy => (pool.quote_reserves, amount - quote.fee - 1),
            Side::Sell => (pool.base_reserves, amount),
        };
        let expected = 10_000.0 * net_input as f64 / (input_reserve as f64 + net_input as f64);
        assert!((quote.price_impact_bps.unwrap() - expected).abs() < 1e-9);
    }
}

#[test]
fn prepared_usdc_route_quote_matches_final_instruction_minimum_on_both_sides() {
    let dex = PumpSwap::new(Arc::new(RpcClient::new_mock("fails".into())));
    let target_pool = test_pool(Pubkey::new_unique(), USDC_MINT);
    let bridge_pool = test_pool(USDC_MINT, WSOL);
    for side in [Side::Buy, Side::Sell] {
        let trade = Trade {
            side,
            ..Trade::buy(
                Pubkey::new_unique(),
                target_pool.base_mint,
                1_000_000,
                300,
                Some(Venue::PumpSwap),
            )
        };
        let prepared = dex
            .prepare_usdc_route(
                &trade,
                Pubkey::new_unique(),
                &target_pool,
                &bridge_pool,
                test_fees(),
            )
            .unwrap();
        let swaps: Vec<_> = prepared
            .instructions
            .iter()
            .filter(|ix| ix.program_id == PROGRAM_ID)
            .collect();
        assert_eq!(swaps.len(), 2);
        let final_min = u64::from_le_bytes(swaps[1].data[16..24].try_into().unwrap());
        assert_eq!(prepared.quote.min_out, final_min);
        assert_eq!(prepared.quote.in_amount, trade.amount);
        let (bridge, target) = dex.route_quotes(&trade, &bridge_pool, &target_pool, &test_fees());
        assert_eq!(
            prepared.quote.price_impact_bps,
            crate::price_impact::combine(bridge.price_impact_bps, target.price_impact_bps,)
        );
        assert!(prepared.quote.price_impact_bps.unwrap() > target.price_impact_bps.unwrap());
        if side == Side::Buy {
            assert_eq!(
                bridge.price_impact_bps,
                crate::price_impact::exact_output(bridge_pool.base_reserves, bridge.min_out,)
            );
        }
        if side == Side::Sell {
            assert_eq!(swaps[1].accounts[4].pubkey, WSOL);
            assert_eq!(prepared.instructions.last().unwrap().data, vec![9]);
        }
    }
}

#[test]
fn selects_swap_route_from_the_target_pool_quote_mint() {
    let mint = Pubkey::new_unique();

    assert_eq!(
        PumpSwap::swap_route(&test_pool(mint, WSOL)).unwrap(),
        SwapRoute::DirectSol
    );
    assert_eq!(
        PumpSwap::swap_route(&test_pool(mint, USDC_MINT)).unwrap(),
        SwapRoute::ViaUsdc
    );
    assert!(PumpSwap::swap_route(&test_pool(mint, Pubkey::new_unique())).is_err());
}

#[test]
fn bridge_buys_exact_usdc_with_a_capped_sol_spend() {
    let wallet = Pubkey::new_unique();
    let trade = Trade::buy(wallet, USDC_MINT, 1_000_000, 300, Some(Venue::PumpSwap));
    let pool = test_pool(USDC_MINT, WSOL);
    let dex = PumpSwap::new(Arc::new(RpcClient::new(String::new())));
    let instructions = dex.buy_instructions(
        &trade,
        DEFAULT_SOL_USDC_POOL,
        &pool,
        test_fees(),
        BuyAmounts::ExactBaseOut {
            base_out: 99_000,
            max_quote_in: 1_000_000,
        },
    );

    assert_eq!(instructions.len(), 6);
    assert_eq!(instructions[2].program_id, SYSTEM_PROGRAM);
    assert_eq!(instructions[3].program_id, TOKEN_PROGRAM);
    assert_eq!(instructions[4].program_id, PROGRAM_ID);
    assert_eq!(instructions[5].program_id, TOKEN_PROGRAM);
    assert_eq!(
        &instructions[4].data[..8],
        &anchor_discriminator(BUY_EXACT_BASE_OUT_IX)
    );
    assert_eq!(
        u64::from_le_bytes(instructions[4].data[8..16].try_into().unwrap()),
        99_000
    );
    assert_eq!(
        u64::from_le_bytes(instructions[4].data[16..24].try_into().unwrap()),
        1_000_000
    );
}

#[test]
fn target_leg_spends_the_exact_bridge_output() {
    let wallet = Pubkey::new_unique();
    let target_mint = Pubkey::new_unique();
    let route = Trade::buy(wallet, target_mint, 10_000_000, 500, Some(Venue::PumpSwap));
    let bridge_pool = test_pool(USDC_MINT, WSOL);
    let target_pool = test_pool(target_mint, USDC_MINT);
    let fees = test_fees();
    let dex = PumpSwap::new(Arc::new(RpcClient::new(String::new())));
    let (bridge_quote, target_quote) = dex.route_quotes(&route, &bridge_pool, &target_pool, &fees);

    let instructions = dex.buy_instructions(
        &route,
        Pubkey::new_unique(),
        &target_pool,
        fees,
        BuyAmounts::ExactQuoteIn {
            quote_in: bridge_quote.min_out,
            min_base_out: target_quote.min_out,
        },
    );
    let swap = instructions.last().unwrap();

    assert_eq!(
        u64::from_le_bytes(swap.data[8..16].try_into().unwrap()),
        bridge_quote.min_out
    );
}

#[test]
fn selling_usdc_on_the_bridge_unwraps_sol_to_the_wallet() {
    let wallet = Pubkey::new_unique();
    let trade = Trade::sell(wallet, USDC_MINT, 100_000, 500, Some(Venue::PumpSwap));
    let pool = test_pool(USDC_MINT, WSOL);
    let dex = PumpSwap::new(Arc::new(RpcClient::new(String::new())));
    let instructions =
        dex.sell_instructions(&trade, DEFAULT_SOL_USDC_POOL, &pool, test_fees(), 900_000);
    assert_eq!(instructions.len(), 3);
    assert_eq!(&instructions[1].data[..8], &anchor_discriminator(SELL_IX));
    assert_eq!(
        u64::from_le_bytes(instructions[1].data[8..16].try_into().unwrap()),
        100_000
    );
    assert_eq!(
        u64::from_le_bytes(instructions[1].data[16..24].try_into().unwrap()),
        900_000
    );
    assert_eq!(
        instructions[2],
        close_account(&ata(&wallet, &WSOL, &TOKEN_PROGRAM), &wallet, &wallet)
    );
}

#[test]
fn sell_route_spends_only_guaranteed_usdc_and_unwraps_sol_last() {
    let wallet = Pubkey::new_unique();
    let mint = Pubkey::new_unique();
    let target_address = Pubkey::new_unique();
    let trade = Trade::sell(wallet, mint, 1_000_000, 500, Some(Venue::PumpSwap));
    let target = test_pool(mint, USDC_MINT);
    let bridge = test_pool(USDC_MINT, WSOL);
    let fees = test_fees();
    let dex = PumpSwap::new(Arc::new(RpcClient::new(String::new())));
    let (bridge_quote, target_quote) = dex.route_quotes(&trade, &bridge, &target, &fees);
    let instructions = dex
        .sell_via_usdc_instructions(&trade, target_address, &target, &bridge, fees)
        .unwrap();

    assert_eq!(instructions.len(), 5);
    let target_sell = &instructions[1];
    let bridge_sell = &instructions[3];
    assert_eq!(&target_sell.data[..8], &anchor_discriminator(SELL_IX));
    assert_eq!(&bridge_sell.data[..8], &anchor_discriminator(SELL_IX));
    assert_eq!(target_sell.accounts[0].pubkey, target_address);
    assert_eq!(bridge_sell.accounts[0].pubkey, DEFAULT_SOL_USDC_POOL);
    assert_eq!(&target_sell.data[8..16], &trade.amount.to_le_bytes());
    assert_eq!(&target_sell.data[16..24], &bridge_sell.data[8..16]);
    assert_eq!(
        &bridge_sell.data[16..24],
        &bridge_quote.min_out.to_le_bytes()
    );
    assert_eq!(
        target_sell.accounts[6].pubkey,
        bridge_sell.accounts[5].pubkey
    );
    assert_eq!(bridge_quote.in_amount, target_quote.min_out);
    assert!(target_quote.expected_out > bridge_quote.in_amount);
    assert_eq!(
        instructions[4],
        close_account(&ata(&wallet, &WSOL, &TOKEN_PROGRAM), &wallet, &wallet)
    );
}

#[test]
fn sell_route_quotes_the_bridge_in_sol_with_split_slippage() {
    let mint = Pubkey::new_unique();
    let trade = Trade::sell(Pubkey::new_unique(), mint, 1_000_000, 500, None);
    let target = test_pool(mint, USDC_MINT);
    let bridge = test_pool(USDC_MINT, WSOL);
    let fees = test_fees();
    let dex = PumpSwap::new(Arc::new(RpcClient::new(String::new())));
    let (bridge_quote, target_quote) = dex.route_quotes(&trade, &bridge, &target, &fees);
    let expected_bridge = dex.compute_quote(&bridge, &fees, Side::Sell, target_quote.min_out, 50);

    assert_eq!(
        target_quote.min_out,
        slippage_down(target_quote.expected_out, 450)
    );
    assert_eq!(bridge_quote.expected_out, expected_bridge.expected_out);
    assert_eq!(bridge_quote.min_out, expected_bridge.min_out);
    assert_eq!(bridge_quote.fee, expected_bridge.fee);
}

#[test]
fn sell_route_rejects_zero_output_on_either_leg() {
    let mint = Pubkey::new_unique();
    let mut target = test_pool(mint, USDC_MINT);
    let mut bridge = test_pool(USDC_MINT, WSOL);
    let dex = PumpSwap::new(Arc::new(RpcClient::new(String::new())));
    let trade = Trade::sell(Pubkey::new_unique(), mint, 1_000_000, 500, None);
    bridge.quote_reserves = 0;
    assert!(
        dex.sell_via_usdc_instructions(&trade, Pubkey::new_unique(), &target, &bridge, test_fees())
            .is_err()
    );
    bridge.quote_reserves = 10_000_000_000_000;
    target.quote_reserves = 0;
    assert!(
        dex.sell_via_usdc_instructions(&trade, Pubkey::new_unique(), &target, &bridge, test_fees())
            .is_err()
    );
}
