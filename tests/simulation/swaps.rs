use std::{env, sync::Arc};

use anyhow::{Context, Result, bail, ensure};
use solana_client::rpc_config::{
    RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig,
};
use solana_commitment_config::CommitmentConfig;
use solana_instruction::Instruction;
use solana_message::{VersionedMessage, v0};
use solana_trading_api::{
    AddressLookupTableAccount, Pubkey, RpcClient, SdkFee, Settlement, Side, Signature, Trade,
    TradingClient, USDC_MINT, Venue, VersionedTransaction,
    dexes::common::{TOKEN_PROGRAM, ata, set_compute_unit_limit, set_compute_unit_price},
};

const RPC_ENV: &str = "SIM_RPC_URL";
const WALLET_ENV: &str = "SIM_WALLET";
const MINT_ENV: &str = "SIM_MINT";
const FEE_WALLET_ENV: &str = "SIM_FEE_WALLET";
const VENUE_ENV: &str = "SIM_VENUE";
const POOL_ENV: &str = "SIM_POOL";
const ALTS_ENV: &str = "SIM_ALTS";
const SELL_AMOUNT_ENV: &str = "SIM_SELL_AMOUNT";
const MAX_TRANSACTION_BYTES: u64 = 1_232;
const COMPUTE_LIMIT: u32 = 1_400_000;

#[tokio::test]
#[ignore = "live RPC simulation only; requires public addresses and existing wallet balances"]
async fn simulate_sol_buy() -> Result<()> {
    simulate_swap(Side::Buy, Settlement::Sol).await
}

#[tokio::test]
#[ignore = "live RPC simulation only; requires public addresses and existing wallet balances"]
async fn simulate_usdc_buy() -> Result<()> {
    simulate_swap(Side::Buy, Settlement::Usdc).await
}

#[tokio::test]
#[ignore = "live RPC simulation only; requires public addresses and existing wallet balances"]
async fn simulate_sol_sell() -> Result<()> {
    simulate_swap(Side::Sell, Settlement::Sol).await
}

#[tokio::test]
#[ignore = "live RPC simulation only; requires public addresses and existing wallet balances"]
async fn simulate_usdc_sell() -> Result<()> {
    simulate_swap(Side::Sell, Settlement::Usdc).await
}

async fn simulate_swap(side: Side, settlement: Settlement) -> Result<()> {
    let trade = configured_trade(side, settlement)?;
    let fee_wallet = required_env(FEE_WALLET_ENV)?.parse()?;
    let rpc = Arc::new(RpcClient::new_with_commitment(
        env::var(RPC_ENV).unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into()),
        CommitmentConfig::confirmed(),
    ));
    let tables = env::var(ALTS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::parse)
        .collect::<Result<Vec<Pubkey>, _>>()?;
    let client = TradingClient::new(rpc.clone(), "http://127.0.0.1:1", None)
        .with_sdk_fee(SdkFee::new(fee_wallet, 100)?)
        .with_shared_lookup_table_addresses(&tables)
        .await?;
    let prepared = client.prepare_swap(&trade).await?;
    let expected_fee = prepared.quote.application_fee;
    let impact = prepared
        .quote
        .price_impact_bps
        .context("route price impact unavailable")?;
    ensure!(
        impact.is_finite() && (0.0..=10_000.0).contains(&impact),
        "invalid price impact"
    );
    ensure!(expected_fee > 0, "increase the amount: fee rounds to zero");
    ensure!(
        prepared.quote.min_out > 0,
        "minimum output must be positive"
    );
    let fee_basis = match side {
        Side::Buy => trade.amount,
        Side::Sell => prepared.quote.min_out + expected_fee,
    };
    ensure!(expected_fee == fee_basis / 100, "incorrect 1% fee");

    let mut lookup_tables = prepared.lookup_tables;
    for table in client.shared_lookup_tables() {
        if !lookup_tables
            .iter()
            .any(|existing| existing.key == table.key)
        {
            lookup_tables.push(table.clone());
        }
    }
    let transaction = unsigned_transaction(&trade.wallet, prepared.instructions, &lookup_tables)?;
    let fee_account = match settlement {
        Settlement::Sol => fee_wallet,
        Settlement::Usdc => ata(&fee_wallet, &USDC_MINT, &TOKEN_PROGRAM),
    };
    // Use a quiet fee wallet: separate RPC snapshots can include unrelated incoming transfers.
    let before = rpc
        .get_account_with_commitment(&fee_account, CommitmentConfig::confirmed())
        .await?;
    let initial_fee_balance = before.value.as_ref().map_or(Ok(0), |account| {
        settlement_balance(settlement, account.lamports, &account.data)
    })?;
    let simulation = rpc
        .simulate_transaction_with_config(
            &transaction,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                commitment: Some(CommitmentConfig::confirmed()),
                min_context_slot: Some(before.context.slot),
                accounts: Some(RpcSimulateTransactionAccountsConfig {
                    encoding: None,
                    addresses: vec![fee_account.to_string()],
                }),
                ..Default::default()
            },
        )
        .await?
        .value;
    if let Some(error) = simulation.err {
        bail!(
            "{side:?}/{settlement:?} simulation failed: {error:?}\n{}",
            simulation.logs.unwrap_or_default().join("\n")
        );
    }
    let accounts = simulation.accounts.context("simulation omitted accounts")?;
    let account = accounts
        .first()
        .and_then(Option::as_ref)
        .context("fee account missing")?;
    let data = account
        .data
        .decode()
        .context("fee account data is not binary")?;
    let final_fee_balance = settlement_balance(settlement, account.lamports, &data)?;
    ensure!(
        final_fee_balance.checked_sub(initial_fee_balance) == Some(expected_fee),
        "fee wallet must receive exactly one fee of {expected_fee}; before={initial_fee_balance}, after={final_fee_balance}"
    );
    println!(
        "{side:?}/{settlement:?}: {} bytes, {:?} CU, fee={expected_fee}, impact={impact:.4} bps, minimum output={}",
        bincode::serialized_size(&transaction)?,
        simulation.units_consumed,
        prepared.quote.min_out
    );
    Ok(())
}

fn configured_trade(side: Side, settlement: Settlement) -> Result<Trade> {
    let venue = match env::var(VENUE_ENV).as_deref().unwrap_or("pumpswap") {
        "pumpfun" => Venue::PumpFun,
        "pumpswap" => Venue::PumpSwap,
        other => bail!("unsupported {VENUE_ENV}: {other}"),
    };
    let amount = match side {
        Side::Buy => 1_000_000, // 0.001 SOL or 1 USDC, depending on settlement.
        Side::Sell => required_env(SELL_AMOUNT_ENV)?
            .parse()
            .context("invalid sell base units")?,
    };
    Ok(Trade {
        wallet: required_env(WALLET_ENV)?.parse()?,
        mint: required_env(MINT_ENV)?.parse()?,
        side,
        settlement,
        amount,
        slippage_bps: 500,
        venue: Some(venue),
        pool: env::var(POOL_ENV)
            .ok()
            .map(|value| value.parse())
            .transpose()?,
    })
}

fn required_env(name: &str) -> Result<String> {
    env::var(name)
        .with_context(|| format!("set {name}; see tests/README.md (no private key needed)"))
}

fn unsigned_transaction(
    payer: &Pubkey,
    instructions: Vec<Instruction>,
    tables: &[AddressLookupTableAccount],
) -> Result<VersionedTransaction> {
    let mut budgeted = vec![
        set_compute_unit_limit(COMPUTE_LIMIT),
        set_compute_unit_price(1_000),
    ];
    budgeted.extend(instructions);
    let message = v0::Message::try_compile(payer, &budgeted, tables, Default::default())?;
    ensure!(
        message.header.num_required_signatures == 1,
        "unexpected extra signer"
    );
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(message),
    };
    let size = bincode::serialized_size(&transaction)?;
    ensure!(
        size <= MAX_TRANSACTION_BYTES,
        "{size} bytes exceeds packet limit; configure {ALTS_ENV}"
    );
    Ok(transaction)
}

fn settlement_balance(settlement: Settlement, lamports: u64, data: &[u8]) -> Result<u64> {
    match settlement {
        Settlement::Sol => Ok(lamports),
        Settlement::Usdc => {
            let amount = data.get(64..72).context("truncated USDC token account")?;
            Ok(u64::from_le_bytes(amount.try_into()?))
        }
    }
}

#[test]
fn simulation_transaction_is_unsigned() -> Result<()> {
    let transaction = unsigned_transaction(&Pubkey::new_unique(), vec![], &[])?;
    assert_eq!(transaction.signatures, vec![Signature::default()]);
    Ok(())
}

#[test]
fn oversized_simulation_is_rejected_before_rpc() {
    let instruction = Instruction {
        program_id: Pubkey::new_unique(),
        accounts: vec![],
        data: vec![0; MAX_TRANSACTION_BYTES as usize],
    };
    let error = unsigned_transaction(&Pubkey::new_unique(), vec![instruction], &[]).unwrap_err();
    assert!(error.to_string().contains("exceeds packet limit"));
}

#[test]
fn usdc_fee_balance_uses_tokens_not_rent_lamports() -> Result<()> {
    let mut data = vec![0; 165];
    data[64..72].copy_from_slice(&10_000_u64.to_le_bytes());
    assert_eq!(
        settlement_balance(Settlement::Usdc, 2_039_280, &data)?,
        10_000
    );
    assert!(settlement_balance(Settlement::Usdc, 2_039_280, &[]).is_err());
    Ok(())
}
