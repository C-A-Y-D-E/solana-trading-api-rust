use super::*;

#[tokio::test]
async fn usdc_sell_output_reads_token_balance_instead_of_sol_balance() {
    let mocks = std::collections::HashMap::from([(
        solana_client::rpc_request::RpcRequest::GetTokenAccountBalance,
        serde_json::json!({"context": {"slot": 1}, "value": {
            "amount": "1234567", "decimals": 6, "uiAmount": 1.234567, "uiAmountString": "1.234567"
        }}),
    )]);
    let rpc = Arc::new(RpcClient::new_mock_with_mocks("succeeds".into(), mocks));
    let trade = Trade::sell(
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        100,
        100,
        Some(crate::Venue::PumpSwap),
    )
    .with_settlement(Settlement::Usdc);
    assert_eq!(output_balance(&rpc, &trade).await, 1_234_567);
}

struct NoBroadcast;

#[async_trait::async_trait]
impl Submitter for NoBroadcast {
    async fn submit(&self, _: &VersionedTransaction) -> anyhow::Result<Signature> {
        panic!("transaction building must not broadcast")
    }
}

#[async_trait::async_trait]
impl Signer for NoBroadcast {
    async fn sign(&self, _: &Pubkey, _: &VersionedTransaction) -> anyhow::Result<Signature> {
        panic!("failed simulation must stop before signing")
    }
}

fn simulation_rpc(value: serde_json::Value) -> Arc<RpcClient> {
    Arc::new(RpcClient::new_mock_with_mocks(
        "succeeds".into(),
        std::collections::HashMap::from([(
            solana_client::rpc_request::RpcRequest::SimulateTransaction,
            value,
        )]),
    ))
}

fn test_prepared(trade: &Trade) -> PreparedSwap {
    PreparedSwap {
        venue: "fixture",
        quote: crate::Quote {
            in_amount: trade.amount,
            price_impact_bps: None,
            expected_out: 100,
            min_out: 90,
            fee: 1,
            application_fee: 0,
        },
        instructions: vec![crate::dexes::common::system_transfer(
            &trade.wallet,
            &Pubkey::new_unique(),
            1,
        )],
        lookup_tables: vec![],
    }
}

#[tokio::test]
async fn simulation_failure_keeps_logs_and_stops_before_signing() {
    let rpc = simulation_rpc(serde_json::json!({"context": {"slot": 1}, "value": {
        "err": {"InstructionError": [0, {"Custom": 6001}]},
        "logs": ["minimum output not met"], "unitsConsumed": 10_000
    }}));
    let trade = Trade::sell(
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        100,
        100,
        Some(crate::Venue::PumpFun),
    );
    let error = submit_swap(
        &rpc,
        test_prepared(&trade),
        &NoBroadcast,
        &NoBroadcast,
        &trade,
        0,
        &[],
    )
    .await
    .unwrap_err();
    let TradeError::Simulation { error, logs } = error else {
        panic!("expected simulation failure")
    };
    assert!(error.contains("6001"));
    assert_eq!(logs, ["minimum output not met"]);
}

#[tokio::test]
async fn simulation_rpc_failure_is_not_treated_as_missing_estimate() {
    let rpc = simulation_rpc(serde_json::Value::Null);
    let trade = Trade::sell(Pubkey::new_unique(), Pubkey::new_unique(), 100, 100, None);
    let error = submit_swap(
        &rpc,
        test_prepared(&trade),
        &NoBroadcast,
        &NoBroadcast,
        &trade,
        0,
        &[],
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        TradeError::Rpc {
            context: "simulate_transaction",
            ..
        }
    ));
}

#[tokio::test]
async fn successful_simulation_uses_estimate_or_default_when_absent() {
    let payer = Pubkey::new_unique();
    for (units, expected_limit) in [(Some(10_000_u64), 12_000_u32), (None, DEFAULT_CU_LIMIT)] {
        let rpc = simulation_rpc(serde_json::json!({"context": {"slot": 1}, "value": {
            "err": null, "unitsConsumed": units
        }}));
        let tx = build_optimal_tx(&rpc, &payer, vec![], &[], 0, &NoBroadcast)
            .await
            .unwrap();
        let VersionedMessage::V0(message) = tx.message else {
            panic!("expected v0")
        };
        assert_eq!(
            message.instructions[0].data,
            set_compute_unit_limit(expected_limit).data
        );
    }
}

#[tokio::test]
async fn multiple_tables_make_large_transaction_fit_without_broadcasting() {
    let rpc = Arc::new(RpcClient::new_mock("succeeds".into()));
    let payer = Pubkey::new_unique();
    let addresses: Vec<_> = (0..40).map(|_| Pubkey::new_unique()).collect();
    let instruction = Instruction {
        program_id: Pubkey::new_unique(),
        accounts: addresses
            .iter()
            .map(|key| solana_instruction::AccountMeta::new(*key, false))
            .collect(),
        data: vec![],
    };
    let error = build_optimal_tx(
        &rpc,
        &payer,
        vec![instruction.clone()],
        &[],
        0,
        &NoBroadcast,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, TradeError::Build(ref message) if message.contains("maximum is 1232")));
    let tables: Vec<_> = addresses
        .chunks(20)
        .map(|chunk| AddressLookupTableAccount {
            key: Pubkey::new_unique(),
            addresses: chunk.to_vec(),
        })
        .collect();
    let transaction = build_optimal_tx(&rpc, &payer, vec![instruction], &tables, 0, &NoBroadcast)
        .await
        .unwrap();
    assert!(bincode::serialized_size(&transaction).unwrap() <= MAX_TRANSACTION_BYTES);
    let VersionedMessage::V0(message) = transaction.message else {
        panic!("expected v0")
    };
    assert_eq!(message.address_table_lookups.len(), 2);
}

#[test]
fn dex_err_preserves_typed_jupiter_error() {
    let typed = TradeError::Http {
        venue: "jupiter",
        status: 401,
        body: "unauthorized".into(),
    };
    let as_anyhow: anyhow::Error = typed.into();
    match dex_err("jupiter", as_anyhow) {
        TradeError::Http { venue, status, .. } => {
            assert_eq!(venue, "jupiter");
            assert_eq!(status, 401);
        }
        other => panic!("expected preserved Http, got {other:?}"),
    }
}

#[test]
fn dex_err_wraps_plain_anyhow_as_venue() {
    match dex_err("pumpfun", anyhow::anyhow!("bonding curve not found")) {
        TradeError::Venue { venue, msg } => {
            assert_eq!(venue, "pumpfun");
            assert!(msg.contains("bonding curve not found"));
        }
        other => panic!("expected Venue, got {other:?}"),
    }
}
