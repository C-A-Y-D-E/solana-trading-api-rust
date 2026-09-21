use super::*;

#[tokio::test]
#[ignore = "live mainnet RPC"]
async fn quote_buy_1_sol() {
    let rpc = Arc::new(RpcClient::new(
        "https://api.mainnet-beta.solana.com".to_string(),
    ));
    let dex = PumpFun::new(rpc);
    let mint: Pubkey = "9JihXt4NZtZzURoMm1KrGN6y2a9LH9xdKkh5p9kJpump"
        .parse()
        .unwrap();

    let params = Trade::buy(
        Pubkey::default(),
        mint,
        1_000_000_000,
        300,
        Some(Venue::PumpFun),
    );

    match dex.quote(&params).await {
        Ok(q) => {
            println!(
                "pumpfun  buy 1 SOL → expected {} tokens (min {}), fee {} lamports",
                q.expected_out, q.min_out, q.fee
            );
            assert!(q.expected_out > 0, "expected nonzero token output");
        }
        Err(e) => println!("pumpfun: no quote (likely graduated to PumpSwap): {e}"),
    }
}

#[tokio::test]
#[ignore = "live mainnet RPC"]
async fn simulate_buy() {
    use solana_client::rpc_config::RpcSimulateTransactionConfig;
    use solana_message::{VersionedMessage, v0};
    use solana_signature::Signature;
    use solana_transaction::versioned::VersionedTransaction;

    let rpc = Arc::new(RpcClient::new(
        "https://api.mainnet-beta.solana.com".to_string(),
    ));
    let dex = PumpFun::new(rpc.clone());
    let mint: Pubkey = "FTNTb1NQeQsizRVmqdc9QrD1oQApyzBB9oGJZwdVpump"
        .parse()
        .unwrap();
    let wallet: Pubkey = "BwuECfotadkbcPqcjjFJfY4khc1MHtLiC3B4gMW1gx5z"
        .parse()
        .unwrap();

    let params = Trade::buy(wallet, mint, 1_000_000, 500, Some(Venue::PumpFun));

    let mut instructions = vec![set_compute_unit_limit(250_000), set_compute_unit_price(0)];
    instructions.extend(dex.swap(&params).await.unwrap().0);

    let blockhash = rpc.get_latest_blockhash().await.unwrap();
    let msg = v0::Message::try_compile(&wallet, &instructions, &[], blockhash).unwrap();
    let tx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(msg),
    };

    let sim = rpc
        .simulate_transaction_with_config(
            &tx,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .value;

    println!(
        "simulate buy (payer {wallet}) → err={:?}, cu={:?}",
        sim.err, sim.units_consumed
    );
    let logs = sim.logs.unwrap_or_default();
    for log in &logs {
        println!("  {log}");
    }

    let reached_program = logs
        .iter()
        .any(|l| l.contains(&format!("{PROGRAM_ID} invoke")));
    assert!(
        sim.err.is_none() || reached_program,
        "pump program not reached — account list looks malformed (err={:?})",
        sim.err
    );
}
