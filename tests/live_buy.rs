use std::sync::Arc;

use async_trait::async_trait;
use solana_keypair::Keypair;

use solana_trading_api::{
    Pubkey, RpcClient, RpcSubmitter, Signature, Signer, Trade, TradingClient, Venue,
    VersionedTransaction,
};

struct KeypairSigner(Keypair);

#[async_trait]
impl Signer for KeypairSigner {
    async fn sign(&self, _wallet: &Pubkey, tx: &VersionedTransaction) -> anyhow::Result<Signature> {
        use solana_signer::Signer as _;
        Ok(self.0.sign_message(&tx.message.serialize()))
    }
}

fn private_key_from_env() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/.env");
    let contents = std::fs::read_to_string(path).expect("create a .env with PRIVATE_KEY=<base58>");
    contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("PRIVATE_KEY="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .expect("PRIVATE_KEY missing from .env")
}

#[tokio::test]
#[ignore = "SENDS A REAL MAINNET TX — spends ~0.001 SOL + ~0.002 SOL ATA rent + fees"]
async fn buy_0_001_sol() {
    use solana_signer::Signer as _;

    let keypair = Keypair::from_base58_string(&private_key_from_env());
    let wallet = keypair.pubkey();

    let rpc = Arc::new(RpcClient::new(
        "https://api.mainnet-beta.solana.com".to_string(),
    ));
    let client = TradingClient::new(rpc.clone(), "https://api.jup.ag", None);
    let mint: Pubkey = "9JihXt4NZtZzURoMm1KrGN6y2a9LH9xdKkh5p9kJpump"
        .parse()
        .unwrap();

    let balance = rpc.get_balance(&wallet).await.unwrap();
    println!("wallet:  {wallet}");
    println!(
        "balance: {} lamports ({:.6} SOL)",
        balance,
        balance as f64 / 1e9
    );

    let trade = Trade::buy(wallet, mint, 1_000_000, 1_000, Some(Venue::PumpFun));

    let quote = client.quote(&trade).await.expect("quote failed");
    println!(
        "quote:   expect {} tokens (min {}), fee {} lamports",
        quote.expected_out, quote.min_out, quote.fee
    );

    let result = client
        .swap(
            &trade,
            &KeypairSigner(keypair),
            &RpcSubmitter::new(rpc.clone()),
            50_000,
        )
        .await
        .expect("swap failed");
    println!("hash:     {}", result.hash);
    println!("status:   {:?}", result.status);
    println!("received: {:?} base tokens", result.amount_received);
    println!("explorer: https://solscan.io/tx/{}", result.hash);
}
