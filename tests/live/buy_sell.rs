use std::{ffi::OsString, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use solana_keypair::{Keypair, read_keypair_file};
use solana_pubkey::pubkey;

use solana_trading_api::{
    Pubkey, RpcClient, RpcSubmitter, SdkFee, Signature, Signer, Trade, TradingClient, Venue,
    VersionedTransaction,
};

struct KeypairSigner(Keypair);

const WALLET_PATH_ENV: &str = "LIVE_WALLET_PATH";
const FEE_WALLET_ENV: &str = "LIVE_FEE_WALLET";
const APPLICATION_FEE_BPS: u16 = 100;
const HOME_ENV: &str = "HOME";
const DEFAULT_WALLET_PATH: &str = ".config/solana/fee-router-admin.json";
const TARGET_POOL: Pubkey = pubkey!("2uF4Xh61rDwxnG9woyxsVQP7zuA6kLFpb3NvnRQeoiSd");
const SHARED_ALT: Pubkey = pubkey!("DG8Y7fV6NaiFBu1LfjNquqbqcPFAVvFA8uP1DhCVC5vb");

#[async_trait]
impl Signer for KeypairSigner {
    async fn sign(&self, _wallet: &Pubkey, tx: &VersionedTransaction) -> anyhow::Result<Signature> {
        use solana_signer::Signer as _;
        Ok(self.0.sign_message(&tx.message.serialize()))
    }
}

fn wallet_path(override_path: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        anyhow::ensure!(!path.is_empty(), "{WALLET_PATH_ENV} must not be empty");
        return Ok(PathBuf::from(path));
    }
    let home = home
        .filter(|value| !value.is_empty())
        .with_context(|| format!("home directory unavailable; set {WALLET_PATH_ENV}"))?;
    Ok(PathBuf::from(home).join(DEFAULT_WALLET_PATH))
}

fn configured_wallet() -> Result<Keypair> {
    let path = wallet_path(
        std::env::var_os(WALLET_PATH_ENV),
        std::env::var_os(HOME_ENV),
    )?;
    // Do not include parser errors that could echo secret key material.
    read_keypair_file(&path).map_err(|_| {
        anyhow!(
            "cannot load Solana keypair JSON at {}; check the path, permissions and file format",
            path.display()
        )
    })
}

fn live_fee(recipient: Option<&str>, wallet: Pubkey) -> Result<SdkFee> {
    let recipient: Pubkey = recipient
        .with_context(|| format!("set {FEE_WALLET_ENV} to your fee recipient's public address"))?
        .parse()
        .with_context(|| format!("{FEE_WALLET_ENV} must be a valid public address"))?;
    anyhow::ensure!(
        recipient != wallet,
        "fee recipient must differ from trading wallet"
    );
    Ok(SdkFee::new(recipient, APPLICATION_FEE_BPS)?)
}

#[tokio::test]
#[ignore = "SENDS REAL MAINNET TXs — buys 0.001 SOL of a token then sells the whole balance back"]
async fn buy_then_sell_all() {
    use solana_signer::Signer as _;

    let keypair = configured_wallet().expect("load live-test wallet");
    let wallet = keypair.pubkey();
    let signer = KeypairSigner(keypair);
    let fee = live_fee(std::env::var(FEE_WALLET_ENV).ok().as_deref(), wallet)
        .expect("configure 1% live-test fee");

    let rpc = Arc::new(RpcClient::new(
        "https://api.mainnet-beta.solana.com".to_string(),
    ));
    let client = TradingClient::new(rpc.clone(), "https://api.jup.ag", None)
        .with_sdk_fee(fee)
        .with_shared_lookup_table_addresses(&[SHARED_ALT])
        .await
        .expect("load shared lookup table");
    let submitter = RpcSubmitter::new(rpc.clone());
    let mint: Pubkey = "pumpCmXqMfrsAkQ5r49WcJnRayYRqmXz6ae8H7H9Dfn"
        .parse()
        .unwrap();

    let balance = rpc.get_balance(&wallet).await.unwrap();
    println!("wallet:  {wallet}");
    println!(
        "fee wallet: {} ({} bps; retained on Jupiter fallback)",
        fee.recipient(),
        fee.basis_points()
    );
    println!(
        "balance: {} lamports ({:.6} SOL)",
        balance,
        balance as f64 / 1e9
    );

    let buy =
        Trade::buy(wallet, mint, 1_000_000, 1_000, Some(Venue::PumpSwap)).with_pool(TARGET_POOL);
    let quote = client.quote(&buy).await.expect("quote failed");
    println!(
        "buy quote: expect {} tokens (min {}), application fee {} lamports",
        quote.expected_out, quote.min_out, quote.application_fee
    );
    assert_eq!(quote.application_fee, buy.amount / 100);
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

    let sell =
        Trade::sell(wallet, mint, amount, 1_000, Some(Venue::PumpSwap)).with_pool(TARGET_POOL);
    let sell_quote = client.quote(&sell).await.expect("sell quote failed");
    println!(
        "sell quote: application fee {} lamports (1% of quoted expected proceeds)",
        sell_quote.application_fee
    );
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

#[test]
fn wallet_defaults_to_existing_solana_config_location() {
    let home = PathBuf::from("test-home");
    assert_eq!(
        wallet_path(None, Some(home.clone().into_os_string())).unwrap(),
        home.join(DEFAULT_WALLET_PATH)
    );
}

#[test]
fn explicit_wallet_path_does_not_require_a_home_directory() {
    let path = PathBuf::from("config/test-wallet.json");
    assert_eq!(
        wallet_path(Some(path.clone().into_os_string()), None).unwrap(),
        path
    );
}

#[test]
fn missing_or_empty_wallet_location_is_rejected() {
    assert!(wallet_path(None, None).is_err());
    assert!(wallet_path(None, Some(OsString::new())).is_err());
    assert!(wallet_path(Some(OsString::new()), Some(OsString::from("test-home"))).is_err());
}

#[test]
fn live_fee_charges_one_percent_to_the_configured_recipient() {
    let recipient = Pubkey::new_unique();
    let fee = live_fee(Some(&recipient.to_string()), Pubkey::new_unique()).unwrap();
    assert_eq!(fee.recipient(), recipient);
    assert_eq!(fee.basis_points(), 100);
}

#[test]
fn live_fee_rejects_missing_invalid_zero_and_self_recipients() {
    let wallet = Pubkey::new_unique();
    for recipient in [None, Some(""), Some("not-an-address")] {
        assert!(live_fee(recipient, wallet).is_err());
    }
    assert!(live_fee(Some(&Pubkey::default().to_string()), wallet).is_err());
    assert!(live_fee(Some(&wallet.to_string()), wallet).is_err());
}
