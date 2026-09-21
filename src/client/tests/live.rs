use super::*;
use solana_pubkey::pubkey;

fn jupiter_api_key() -> Option<String> {
    let env = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/.env")).ok()?;
    env.lines().find_map(|l| {
        l.trim()
            .strip_prefix("JUPITER_API_KEY=")
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

#[tokio::test]
#[ignore = "live: needs JUPITER_API_KEY in .env"]
async fn jupiter_quote() {
    let Some(key) = jupiter_api_key() else {
        println!("skip: no JUPITER_API_KEY in .env");
        return;
    };
    let rpc = Arc::new(RpcClient::new(
        "https://api.mainnet-beta.solana.com".to_string(),
    ));
    let client = TradingClient::new(rpc, "https://api.jup.ag", Some(key));

    let trade = Trade::buy(
        pubkey!("11111111111111111111111111111111"),
        pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
        1_000_000,
        100,
        None,
    );
    let q = client.quote(&trade).await.expect("jupiter quote failed");
    println!(
        "jupiter buy 0.001 SOL → USDC: expect {} (min {})",
        q.expected_out, q.min_out
    );
    assert!(q.expected_out > 0);
}
