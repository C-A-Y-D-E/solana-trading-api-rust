use std::sync::Arc;
use std::time::{Duration, Instant};

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::Instruction;
use solana_message::{AddressLookupTableAccount, VersionedMessage, v0};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

use crate::dexes::common::{ata, set_compute_unit_limit, set_compute_unit_price, tip as tip_ix};
use crate::error::{Result, TradeError};
use crate::types::{Dex, Side, Signer, Submitter, SwapResult, SwapStatus, Trade};

const CU_LIMIT_MAX: u32 = 1_400_000;

const DEFAULT_CU_LIMIT: u32 = 350_000;
const MAX_TRANSACTION_BYTES: u64 = 1_232;

pub(crate) fn dex_err(venue: &'static str, e: anyhow::Error) -> TradeError {
    match e.downcast::<TradeError>() {
        Ok(te) => te,
        Err(e) => TradeError::Venue {
            venue,
            msg: format!("{e:#}"),
        },
    }
}

pub(crate) async fn submit_swap(
    rpc: &Arc<RpcClient>,
    dex: &dyn Dex,
    signer: &dyn Signer,
    submitter: &dyn Submitter,
    params: &Trade,
    priority_fee_lamports: u64,
    supplemental_lookup_tables: &[AddressLookupTableAccount],
) -> Result<SwapResult> {
    let (instructions, mut alts) = dex.swap(params).await.map_err(|e| dex_err(dex.name(), e))?;
    for lookup_table in supplemental_lookup_tables {
        if !alts.iter().any(|existing| existing.key == lookup_table.key) {
            alts.push(lookup_table.clone());
        }
    }
    let mut tx = build_optimal_tx(
        rpc,
        &params.wallet,
        instructions,
        &alts,
        priority_fee_lamports,
        submitter,
    )
    .await?;
    tx.signatures[0] = signer
        .sign(&params.wallet, &tx)
        .await
        .map_err(|e| TradeError::Sign(format!("{e:#}")))?;
    let sig = submitter
        .submit(&tx)
        .await
        .map_err(|e| TradeError::Submit(format!("{e:#}")))?;
    Ok(SwapResult {
        hash: sig.to_string(),
        dex: dex.name(),
        status: SwapStatus::Pending,
        amount_received: None,
    })
}

pub(crate) async fn build_optimal_tx(
    rpc: &Arc<RpcClient>,
    payer: &Pubkey,
    mut instructions: Vec<Instruction>,
    lookup_tables: &[AddressLookupTableAccount],
    priority_fee_lamports: u64,
    submitter: &dyn Submitter,
) -> Result<VersionedTransaction> {
    if let Some(t) = submitter.default_tip() {
        instructions.push(tip_ix(payer, &t.account, t.lamports));
    }

    let cu_limit = match simulate_units(rpc, payer, &instructions, lookup_tables).await {
        Some(units) => ((units as f64 * 1.2) as u32).clamp(1, CU_LIMIT_MAX),
        None => DEFAULT_CU_LIMIT,
    };

    let price = (priority_fee_lamports as u128 * 1_000_000 / cu_limit as u128) as u64;

    let mut all = vec![
        set_compute_unit_limit(cu_limit),
        set_compute_unit_price(price),
    ];
    all.extend(instructions);

    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|source| TradeError::Rpc {
            context: "get_latest_blockhash",
            source,
        })?;
    let msg = v0::Message::try_compile(payer, &all, lookup_tables, blockhash)
        .map_err(|e| TradeError::Build(format!("compile message: {e:?}")))?;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(msg),
    };
    let serialized_size = bincode::serialized_size(&tx)
        .map_err(|e| TradeError::Build(format!("measure transaction: {e}")))?;
    if serialized_size > MAX_TRANSACTION_BYTES {
        return Err(TradeError::Build(format!(
            "transaction is {serialized_size} bytes; maximum is {MAX_TRANSACTION_BYTES}; provide an address lookup table"
        )));
    }
    Ok(tx)
}

async fn simulate_units(
    rpc: &Arc<RpcClient>,
    payer: &Pubkey,
    instructions: &[Instruction],
    lookup_tables: &[AddressLookupTableAccount],
) -> Option<u64> {
    let mut sim_ixs = vec![set_compute_unit_limit(CU_LIMIT_MAX)];
    sim_ixs.extend_from_slice(instructions);
    let blockhash = rpc.get_latest_blockhash().await.ok()?;
    let msg = v0::Message::try_compile(payer, &sim_ixs, lookup_tables, blockhash).ok()?;
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
        .ok()?
        .value;
    if sim.err.is_some() {
        return None;
    }
    sim.units_consumed
}

pub(crate) async fn check_status(rpc: &Arc<RpcClient>, sig: &Signature) -> Result<SwapStatus> {
    let status = rpc
        .get_signature_status_with_commitment(sig, CommitmentConfig::confirmed())
        .await
        .map_err(|source| TradeError::Rpc {
            context: "signature status",
            source,
        })?;
    Ok(match status {
        Some(Ok(())) => SwapStatus::Confirmed,
        Some(Err(_)) => SwapStatus::Failed,
        None => SwapStatus::Pending,
    })
}

pub(crate) async fn confirm(
    rpc: &Arc<RpcClient>,
    sig: &Signature,
    deadline: Duration,
) -> Result<SwapStatus> {
    let start = Instant::now();
    loop {
        match check_status(rpc, sig).await? {
            SwapStatus::Pending => {
                if start.elapsed() >= deadline {
                    return Ok(SwapStatus::Pending);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            terminal => return Ok(terminal),
        }
    }
}

pub(crate) async fn output_balance(rpc: &Arc<RpcClient>, p: &Trade) -> u64 {
    match p.side {
        Side::Buy => {
            let Ok(mint_account) = rpc.get_account(&p.mint).await else {
                return 0;
            };
            let token_account = ata(&p.wallet, &p.mint, &mint_account.owner);
            rpc.get_token_account_balance(&token_account)
                .await
                .ok()
                .and_then(|b| b.amount.parse().ok())
                .unwrap_or(0)
        }
        Side::Sell => rpc.get_balance(&p.wallet).await.unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
