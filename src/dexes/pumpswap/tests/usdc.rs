use super::*;
use crate::{SdkFee, Venue};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;
use solana_client::rpc_request::RpcRequest;

fn pool_rpc(mint: Pubkey) -> Arc<RpcClient> {
    let pool = PoolAccount {
        discriminator: 0,
        pool_bump: 0,
        index: 0,
        creator: Pubkey::new_unique(),
        base_mint: mint,
        quote_mint: USDC_MINT,
        lp_mint: Pubkey::new_unique(),
        pool_base_token_account: Pubkey::new_unique(),
        pool_quote_token_account: Pubkey::new_unique(),
        lp_supply: 0,
        coin_creator: Pubkey::default(),
        is_mayhem_mode: false,
        is_cashback_coin: false,
    };
    let account = json!({"lamports": 1_000_000, "owner": PROGRAM_ID.to_string(), "executable": false,
        "rentEpoch": 0, "data": [STANDARD.encode(borsh::to_vec(&pool).unwrap()), "base64"]});
    let mint_account = json!({"lamports": 1_000_000, "owner": TOKEN_PROGRAM.to_string(), "executable": false,
        "rentEpoch": 0, "data": ["", "base64"]});
    let balance = json!({"context": {"slot": 1}, "value": {
        "amount": "1000000000000", "decimals": 6, "uiAmount": 1_000_000.0, "uiAmountString": "1000000"
    }});
    let mocks = vec![
        (
            RpcRequest::GetAccountInfo,
            json!({"context": {"slot": 1}, "value": account.clone()}),
        ),
        (
            RpcRequest::GetAccountInfo,
            json!({"context": {"slot": 1}, "value": account}),
        ),
        (
            RpcRequest::GetMultipleAccounts,
            json!({"context": {"slot": 1}, "value": [mint_account.clone(), mint_account]}),
        ),
        (RpcRequest::GetTokenAccountBalance, balance.clone()),
        (RpcRequest::GetTokenAccountBalance, balance),
    ]
    .into_iter()
    .collect();
    Arc::new(RpcClient::new_mock_with_mocks_map("succeeds", mocks))
}

#[tokio::test]
async fn usdc_pool_prepares_one_swap_and_usdc_fee_without_sol_bridge() {
    for side in [Side::Buy, Side::Sell] {
        let mint = Pubkey::new_unique();
        let dex = PumpSwap::new(pool_rpc(mint));
        assert!(
            dex.fee_settings
                .set(FeeSettings {
                    standard_protocol_recipient: Pubkey::new_unique(),
                    mayhem_protocol_recipient: Pubkey::new_unique(),
                    buyback_recipient: Pubkey::new_unique(),
                    lp_bps: 20,
                    protocol_bps: 5,
                    creator_bps: 0,
                })
                .is_ok()
        );
        let trade = Trade {
            side,
            ..Trade::buy(
                Pubkey::new_unique(),
                mint,
                1_000_000,
                100,
                Some(Venue::PumpSwap),
            )
            .with_pool(Pubkey::new_unique())
            .with_settlement(Settlement::Usdc)
        };
        assert_eq!(dex.quote_mint(&trade).await.unwrap(), USDC_MINT);
        let fee = SdkFee::new(Pubkey::new_unique(), 100).unwrap();
        let adjusted = fee.venue_trade(&trade).unwrap();
        let prepared = dex.prepare_swap(&adjusted).await.unwrap();
        let gross_min = prepared.quote.min_out;
        let prepared = fee.apply(&trade, prepared).unwrap();
        let swaps: Vec<_> = prepared
            .instructions
            .iter()
            .filter(|ix| ix.program_id == PROGRAM_ID)
            .collect();
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].accounts[4].pubkey, USDC_MINT);
        assert_eq!(
            u64::from_le_bytes(swaps[0].data[8..16].try_into().unwrap()),
            adjusted.amount
        );
        assert_eq!(
            u64::from_le_bytes(swaps[0].data[16..24].try_into().unwrap()),
            gross_min
        );
        assert!(
            !prepared
                .instructions
                .iter()
                .any(|ix| ix.program_id == SYSTEM_PROGRAM)
        );
        assert!(
            !prepared
                .instructions
                .iter()
                .any(|ix| ix.program_id == TOKEN_PROGRAM && ix.data == [9])
        );
        let expected_fee = if side == Side::Buy {
            10_000
        } else {
            gross_min / 100
        };
        assert_eq!(prepared.quote.application_fee, expected_fee);
        assert_eq!(
            prepared.instructions.last().unwrap().program_id,
            TOKEN_PROGRAM
        );
    }
}
