use std::sync::Arc;

use solana_trading_api::{RpcClient, TradingClient};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rpc = Arc::new(RpcClient::new(
        "https://api.mainnet-beta.solana.com".to_string(),
    ));

    let _client = TradingClient::new(rpc, "https://api.jup.ag", None);
    println!("trading client ready: pumpfun, pumpswap, jupiter");
    Ok(())
}
