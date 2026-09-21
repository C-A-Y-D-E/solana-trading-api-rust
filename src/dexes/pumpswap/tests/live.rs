use super::*;

#[tokio::test]
#[ignore = "live mainnet RPC"]
async fn simulate_buy_from_usdc_pool() {
    let rpc = Arc::new(RpcClient::new(
        "https://api.mainnet-beta.solana.com".to_string(),
    ));
    let dex = PumpSwap::new(rpc);
    let wallet: Pubkey = "HPkBhdBS8tEfHbsWK1v2f82cPYrXHKZyr29apDbTttuD"
        .parse()
        .unwrap();
    let mint: Pubkey = "aHwwJn74ttpoHxzsrc1UhNHSjxyDAggh1sULqC3pump"
        .parse()
        .unwrap();
    let pool: Pubkey = "EwYm6KmxzpWuwthzAAMd3ND8Bp5TJX9hWnArJisV2TPQ"
        .parse()
        .unwrap();
    let trade = Trade::buy(wallet, mint, 1_000_000, 500, Some(Venue::PumpSwap)).with_pool(pool);

    let quote = dex.quote(&trade).await.unwrap();
    let (instructions, _) = dex.swap(&trade).await.unwrap();
    let swaps = instructions
        .iter()
        .filter(|instruction| instruction.program_id == PROGRAM_ID)
        .collect::<Vec<_>>();

    assert!(quote.expected_out > 0);
    assert!(quote.min_out > 0);
    assert_eq!(swaps.len(), 2);
    assert_eq!(
        &swaps[0].data[..8],
        &anchor_discriminator(BUY_EXACT_BASE_OUT_IX)
    );
    assert_eq!(
        &swaps[1].data[..8],
        &anchor_discriminator(BUY_EXACT_QUOTE_IN_IX)
    );
    assert_eq!(&swaps[0].data[8..16], &swaps[1].data[8..16]);

    let mut execution_instructions = vec![
        set_compute_unit_limit(350_000),
        set_compute_unit_price(50_000),
    ];
    execution_instructions.extend(instructions.clone());
    execution_instructions.push(tip(
        &wallet,
        &crate::submit::BloxrouteSubmitter::DEFAULT_TIP_ACCOUNT,
        crate::submit::BloxrouteSubmitter::MIN_TIP_LAMPORTS,
    ));
    let blockhash = dex.rpc.get_latest_blockhash().await.unwrap();
    let message =
        solana_message::v0::Message::try_compile(&wallet, &execution_instructions, &[], blockhash)
            .unwrap();
    let transaction = solana_transaction::versioned::VersionedTransaction {
        signatures: vec![solana_signature::Signature::default()],
        message: solana_message::VersionedMessage::V0(message),
    };
    assert!(bincode::serialized_size(&transaction).unwrap() > 1_232);

    let lookup_addresses = crate::shared_lookup_addresses(dex.rpc.clone(), DEFAULT_SOL_USDC_POOL)
        .await
        .unwrap();
    assert!(!lookup_addresses.contains(&pool));
    assert!(!lookup_addresses.contains(&mint));
    assert!(!lookup_addresses.contains(&wallet));
    let lookup_table = AddressLookupTableAccount {
        key: Pubkey::new_unique(),
        addresses: lookup_addresses,
    };
    let compressed_message = solana_message::v0::Message::try_compile(
        &wallet,
        &execution_instructions,
        std::slice::from_ref(&lookup_table),
        blockhash,
    )
    .unwrap();
    let compressed_transaction = solana_transaction::versioned::VersionedTransaction {
        signatures: vec![solana_signature::Signature::default()],
        message: solana_message::VersionedMessage::V0(compressed_message),
    };
    assert!(bincode::serialized_size(&compressed_transaction).unwrap() <= 1_232);
    println!(
        "shared ALT transaction size: {} ({} reusable addresses)",
        bincode::serialized_size(&compressed_transaction).unwrap(),
        lookup_table.addresses.len()
    );
    let other_wallet = Pubkey::new_unique();
    let mut other_instructions = execution_instructions.clone();
    // Replace every wallet-derived address to prove the table is not tied to one user.
    let mut other_trade = trade;
    other_trade.wallet = other_wallet;
    other_instructions.splice(
        2..2 + instructions.len(),
        dex.swap(&other_trade).await.unwrap().0,
    );
    *other_instructions.last_mut().unwrap() = tip(
        &other_wallet,
        &crate::BloxrouteSubmitter::DEFAULT_TIP_ACCOUNT,
        crate::BloxrouteSubmitter::MIN_TIP_LAMPORTS,
    );
    let other_message = solana_message::v0::Message::try_compile(
        &other_wallet,
        &other_instructions,
        &[lookup_table],
        blockhash,
    )
    .unwrap();
    let other_transaction = solana_transaction::versioned::VersionedTransaction {
        signatures: vec![solana_signature::Signature::default()],
        message: solana_message::VersionedMessage::V0(other_message),
    };
    assert!(bincode::serialized_size(&other_transaction).unwrap() <= 1_232);

    // Public tables are test fixtures only; their owners can deactivate them.
    let mut public_lookup_tables = Vec::new();
    for address in [
        "9wfFYYUnyXubYcLt1MWNVzS4KXZ2zsojuzAd3bSTU6Jo",
        "6Nv8PjtF6xymKEBFNhjkSDw1Gbsd9MChE1VmWZsrw6qd",
    ] {
        public_lookup_tables.push(
            crate::load_address_lookup_table(dex.rpc.as_ref(), address.parse().unwrap())
                .await
                .unwrap(),
        );
    }
    let public_message = solana_message::v0::Message::try_compile(
        &wallet,
        &execution_instructions,
        &public_lookup_tables,
        blockhash,
    )
    .unwrap();
    let public_transaction = solana_transaction::versioned::VersionedTransaction {
        signatures: vec![solana_signature::Signature::default()],
        message: solana_message::VersionedMessage::V0(public_message),
    };
    println!(
        "public ALT transaction size: {}",
        bincode::serialized_size(&public_transaction).unwrap()
    );
    assert!(bincode::serialized_size(&public_transaction).unwrap() <= 1_232);
    let simulation = dex
        .rpc
        .simulate_transaction_with_config(
            &public_transaction,
            solana_client::rpc_config::RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .value;
    for log in simulation.logs.unwrap_or_default() {
        println!("{log}");
    }
    println!("simulation compute units: {:?}", simulation.units_consumed);
    assert_eq!(simulation.err, None);
}

#[tokio::test]
#[ignore = "live mainnet RPC"]
async fn quote_buy_1_sol() {
    let rpc = Arc::new(RpcClient::new(
        "https://api.mainnet-beta.solana.com".to_string(),
    ));
    let dex = PumpSwap::new(rpc);
    let mint: Pubkey = "9JihXt4NZtZzURoMm1KrGN6y2a9LH9xdKkh5p9kJpump"
        .parse()
        .unwrap();

    let params = Trade::buy(
        Pubkey::default(),
        mint,
        1_000_000_000,
        300,
        Some(Venue::PumpSwap),
    );

    match dex.quote(&params).await {
        Ok(q) => {
            println!(
                "pumpswap buy 1 SOL → expected {} tokens (min {}), fee {} lamports",
                q.expected_out, q.min_out, q.fee
            );
            assert!(q.expected_out > 0, "expected nonzero token output");
        }
        Err(e) => {
            println!("pumpswap: no quote (not a canonical WSOL pool, or still on the curve): {e}")
        }
    }
}

#[tokio::test]
#[ignore = "live mainnet RPC"]
async fn simulate_mayhem_buy() {
    use solana_client::rpc_config::RpcSimulateTransactionConfig;
    use solana_message::{VersionedMessage, v0};
    use solana_signature::Signature;
    use solana_transaction::versioned::VersionedTransaction;

    let rpc = Arc::new(RpcClient::new(
        "https://api.mainnet-beta.solana.com".to_string(),
    ));
    let dex = PumpSwap::new(rpc.clone());
    let mint: Pubkey = "HXTaBKp2qa5n2DAzzc949tAJMmuVyRoCQCDNkFKkpump"
        .parse()
        .unwrap();
    let pool: Pubkey = "7aN5B42L5bLTvxoGLCScdqU46o4C1j4zKxmjjrmwQKuR"
        .parse()
        .unwrap();
    let wallet: Pubkey = "7a1xV8pUaJbUMqGVC3Z2NQbhW5pBJT2UXfiSFtuUC18S"
        .parse()
        .unwrap();

    assert!(dex.load_pool(&pool, &mint).await.unwrap().is_mayhem);

    let params = Trade::buy(wallet, mint, 100_000, 500, Some(Venue::PumpSwap)).with_pool(pool);
    let mut instructions = vec![set_compute_unit_limit(350_000), set_compute_unit_price(0)];
    instructions.extend(dex.swap(&params).await.unwrap().0);

    let blockhash = rpc.get_latest_blockhash().await.unwrap();
    let msg = v0::Message::try_compile(&wallet, &instructions, &[], blockhash).unwrap();
    let tx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(msg),
    };
    let simulation = rpc
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

    for log in simulation.logs.unwrap_or_default() {
        println!("  {log}");
    }
    assert_eq!(simulation.err, None);
}
