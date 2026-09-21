use super::*;
use crate::types::Venue;
use solana_client::rpc_config::{
    RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig,
};
use solana_message::AddressLookupTableAccount;
use solana_message::{VersionedMessage, v0};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

const WALLET: Pubkey = pubkey!("HPkBhdBS8tEfHbsWK1v2f82cPYrXHKZyr29apDbTttuD");
const MINT: Pubkey = pubkey!("aHwwJn74ttpoHxzsrc1UhNHSjxyDAggh1sULqC3pump");
const TARGET_POOL: Pubkey = pubkey!("EwYm6KmxzpWuwthzAAMd3ND8Bp5TJX9hWnArJisV2TPQ");
const SHARED_ALT: Pubkey = pubkey!("DG8Y7fV6NaiFBu1LfjNquqbqcPFAVvFA8uP1DhCVC5vb");

fn transaction(
    instructions: &[Instruction],
    table: &AddressLookupTableAccount,
) -> VersionedTransaction {
    let message = v0::Message::try_compile(
        &WALLET,
        instructions,
        std::slice::from_ref(table),
        Default::default(),
    )
    .unwrap();
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(message),
    };
    let size = bincode::serialized_size(&transaction).unwrap();
    println!("transaction size: {size} bytes");
    assert!(size <= 1_232, "transaction exceeds the packet limit");
    transaction
}

#[tokio::test]
#[ignore = "live mainnet RPC; simulation only, no private key or broadcast"]
async fn simulate_sell_from_usdc_pool() {
    let rpc = Arc::new(RpcClient::new("https://api.mainnet-beta.solana.com".into()));
    let dex = PumpSwap::new(rpc.clone());
    let table = crate::load_address_lookup_table(&rpc, SHARED_ALT)
        .await
        .unwrap();
    let buy =
        Trade::buy(WALLET, MINT, 1_000_000, 500, Some(Venue::PumpSwap)).with_pool(TARGET_POOL);
    let buy_quote = dex.quote(&buy).await.unwrap();
    let sell = Trade::sell(WALLET, MINT, buy_quote.min_out, 500, Some(Venue::PumpSwap))
        .with_pool(TARGET_POOL);
    let sell_quote = dex.quote(&sell).await.unwrap();
    assert_eq!(sell_quote.in_amount, sell.amount);
    assert!(sell_quote.min_out > 0);
    let (sell_instructions, _) = dex.swap(&sell).await.unwrap();
    let swaps: Vec<_> = sell_instructions
        .iter()
        .filter(|instruction| instruction.program_id == PROGRAM_ID)
        .collect();
    assert_eq!(swaps.len(), 2);
    for swap in &swaps {
        assert_eq!(&swap.data[..8], &anchor_discriminator(SELL_IX));
    }
    assert_eq!(&swaps[0].data[16..24], &swaps[1].data[8..16]);

    let tip_instruction = tip(
        &WALLET,
        &crate::BloxrouteSubmitter::DEFAULT_TIP_ACCOUNT,
        crate::BloxrouteSubmitter::MIN_TIP_LAMPORTS,
    );
    let mut sell_execution = vec![
        set_compute_unit_limit(350_000),
        set_compute_unit_price(50_000),
    ];
    sell_execution.extend(sell_instructions.clone());
    sell_execution.push(tip_instruction.clone());
    println!("sell-only transaction with shared ALT {SHARED_ALT}");
    transaction(&sell_execution, &table);

    // Seed tokens with a simulated buy because the fixture wallet may hold none on-chain.
    let mut round_trip = vec![
        set_compute_unit_limit(600_000),
        set_compute_unit_price(50_000),
    ];
    round_trip.extend(dex.swap(&buy).await.unwrap().0);
    round_trip.extend(sell_instructions);
    round_trip.push(tip_instruction);
    let simulated_transaction = transaction(&round_trip, &table);
    let usdc_ata = ata(&WALLET, &USDC_MINT, &TOKEN_PROGRAM);
    let wsol_ata = ata(&WALLET, &WSOL, &TOKEN_PROGRAM);
    let before = rpc.get_multiple_accounts(&[usdc_ata]).await.unwrap();
    let initial_usdc = before[0]
        .as_ref()
        .map_or(0, |account| token_amount(&account.data));
    let simulation = rpc
        .simulate_transaction_with_config(
            &simulated_transaction,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                accounts: Some(RpcSimulateTransactionAccountsConfig {
                    encoding: None,
                    addresses: vec![usdc_ata.to_string(), wsol_ata.to_string()],
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .value;
    let logs = simulation.logs.unwrap_or_default();
    if simulation.err.is_some() {
        for log in &logs {
            println!("{log}");
        }
    }
    println!("round-trip compute units: {:?}", simulation.units_consumed);
    assert_eq!(simulation.err, None);
    let accounts = simulation.accounts.unwrap();
    let remaining_usdc = token_amount(&accounts[0].as_ref().unwrap().data.decode().unwrap());
    assert!(
        remaining_usdc >= initial_usdc,
        "must not spend pre-existing USDC"
    );
    if let Some(closed_wsol) = &accounts[1] {
        println!("post-simulation WSOL account: {closed_wsol:?}");
        assert_eq!(closed_wsol.lamports, 0, "WSOL must be unwrapped");
        assert!(closed_wsol.data.decode().unwrap().is_empty());
        assert_eq!(closed_wsol.owner, SYSTEM_PROGRAM.to_string());
    }
    println!("surplus USDC base units: {}", remaining_usdc - initial_usdc);
}

fn token_amount(data: &[u8]) -> u64 {
    u64::from_le_bytes(data[64..72].try_into().unwrap())
}
