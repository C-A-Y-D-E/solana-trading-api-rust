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
#[ignore = "SENDS REAL MAINNET TXs — buys 0.001 SOL of a token then sells the whole balance back"]
async fn buy_then_sell_all() {
    use solana_signer::Signer as _;

    let keypair = Keypair::from_base58_string(&private_key_from_env());
    let wallet = keypair.pubkey();
    let signer = KeypairSigner(keypair);

    let rpc = Arc::new(RpcClient::new(
        "https://api.mainnet-beta.solana.com".to_string(),
    ));
    let client = TradingClient::new(rpc.clone(), "https://api.jup.ag", None);
    let submitter = RpcSubmitter::new(rpc.clone());
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

    let buy = Trade::buy(wallet, mint, 1_000_000, 1_000, Some(Venue::PumpFun));
    let quote = client.quote(&buy).await.expect("quote failed");
    println!(
        "buy quote: expect {} tokens (min {})",
        quote.expected_out, quote.min_out
    );
    let buy_res = client
        .swap(&buy, &signer, &submitter, 50_000)
        .await
        .expect("buy failed");
    println!(
        "buy:  {} [{:?}] received {:?}",
        buy_res.hash, buy_res.status, buy_res.amount_received
    );
    println!("      https://solscan.io/tx/{}", buy_res.hash);

    let amount = client.token_balance(&wallet, &mint).await.unwrap_or(0);
    println!("token balance: {amount}");
    if amount == 0 {
        println!("nothing to sell (buy may not have landed yet)");
        return;
    }

    // sell via Jupiter (venue None): it sizes the sell to the curve's real liquidity, unlike
    // the raw pump path which overflows (6024) when a full exit exceeds the curve's SOL.
    let sell = Trade::sell(wallet, mint, amount, 1_000, Some(Venue::PumpFun));
    let sell_res = client
        .swap(&sell, &signer, &submitter, 50_000)
        .await
        .expect("sell failed");
    println!(
        "sell: {} [{:?}] received {:?}",
        sell_res.hash, sell_res.status, sell_res.amount_received
    );
    println!("      https://solscan.io/tx/{}", sell_res.hash);
}
