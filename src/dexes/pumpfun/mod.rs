use std::sync::{Arc, OnceLock};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use borsh::BorshDeserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::AddressLookupTableAccount;
use solana_pubkey::{Pubkey, pubkey};

use crate::dexes::common::*;
use crate::types::{Dex, Quote, Side, Trade};

pub const PROGRAM_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
pub const FEE_PROGRAM_ID: Pubkey = pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

const BUYBACK_RECIPIENT: Pubkey = pubkey!("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD");

const BUY_IX: &str = "buy_exact_sol_in";
const SELL_IX: &str = "sell";

#[derive(BorshDeserialize)]
#[allow(dead_code)]
struct BondingCurveAccount {
    discriminator: u64,
    virtual_token_reserves: u64,
    virtual_quote_reserves: u64,
    real_token_reserves: u64,
    real_quote_reserves: u64,
    token_total_supply: u64,
    complete: bool,
    creator: Pubkey,
}

#[derive(BorshDeserialize)]
#[allow(dead_code)]
struct GlobalAccount {
    discriminator: u64,
    initialized: bool,
    authority: Pubkey,
    fee_recipient: Pubkey,
    initial_virtual_token_reserves: u64,
    initial_virtual_sol_reserves: u64,
    initial_real_token_reserves: u64,
    token_total_supply: u64,
    fee_basis_points: u64,
    withdraw_authority: Pubkey,
    enable_migrate: bool,
    pool_migration_fee: u64,
    creator_fee_basis_points: u64,
}

#[derive(Clone, Copy)]
struct FeeSettings {
    recipient: Pubkey,
    protocol_bps: u64,
    creator_bps: u64,
}

struct Curve {
    creator: Pubkey,
    base_reserves: u64,
    quote_reserves: u64,
    real_token_reserves: u64,
    token_program: Pubkey,
}

pub struct PumpFun {
    rpc: Arc<RpcClient>,
    fee_settings: OnceLock<FeeSettings>,
}

impl PumpFun {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self {
            rpc,
            fee_settings: OnceLock::new(),
        }
    }

    pub fn bonding_curve_pda(mint: &Pubkey) -> Pubkey {
        pda(&[b"bonding-curve", mint.as_ref()], &PROGRAM_ID)
    }

    fn global_pda() -> Pubkey {
        pda(&[b"global"], &PROGRAM_ID)
    }
    fn event_authority_pda() -> Pubkey {
        pda(&[b"__event_authority"], &PROGRAM_ID)
    }
    fn creator_vault_pda(creator: &Pubkey) -> Pubkey {
        pda(&[b"creator-vault", creator.as_ref()], &PROGRAM_ID)
    }
    fn global_volume_pda() -> Pubkey {
        pda(&[b"global_volume_accumulator"], &PROGRAM_ID)
    }
    fn user_volume_pda(user: &Pubkey) -> Pubkey {
        pda(&[b"user_volume_accumulator", user.as_ref()], &PROGRAM_ID)
    }
    fn fee_config_pda() -> Pubkey {
        pda(&[b"fee_config", PROGRAM_ID.as_ref()], &FEE_PROGRAM_ID)
    }
    fn bonding_curve_v2_pda(mint: &Pubkey) -> Pubkey {
        pda(&[b"bonding-curve-v2", mint.as_ref()], &PROGRAM_ID)
    }

    async fn fee_settings(&self) -> Result<FeeSettings> {
        if let Some(f) = self.fee_settings.get() {
            return Ok(*f);
        }
        let acc = self.rpc.get_account(&Self::global_pda()).await?;
        let global: GlobalAccount = decode_account(&acc.data)?;
        let f = FeeSettings {
            recipient: global.fee_recipient,
            protocol_bps: global.fee_basis_points,
            creator_bps: global.creator_fee_basis_points,
        };
        let _ = self.fee_settings.set(f);
        Ok(f)
    }

    async fn load_curve(&self, curve_address: &Pubkey, mint: &Pubkey) -> Result<Curve> {
        let accounts = self
            .rpc
            .get_multiple_accounts(&[*curve_address, *mint])
            .await?;
        let curve_acc = accounts[0]
            .as_ref()
            .ok_or_else(|| anyhow!("pumpfun: bonding curve not found: {curve_address}"))?;
        let mint_acc = accounts[1]
            .as_ref()
            .ok_or_else(|| anyhow!("pumpfun: mint not found: {mint}"))?;
        let c: BondingCurveAccount = decode_account(&curve_acc.data)
            .map_err(|e| anyhow!("pumpfun: bad bonding curve {curve_address}: {e}"))?;
        if c.complete {
            return Err(anyhow!(
                "pumpfun: bonding curve complete — {mint} graduated; trade it via PumpSwap/Jupiter"
            ));
        }
        Ok(Curve {
            creator: c.creator,
            base_reserves: c.virtual_token_reserves,
            quote_reserves: c.virtual_quote_reserves,
            real_token_reserves: c.real_token_reserves,
            token_program: mint_acc.owner,
        })
    }

    fn compute_quote(
        &self,
        curve: &Curve,
        fees: &FeeSettings,
        side: Side,
        amount: u64,
        slippage_bps: u64,
    ) -> Quote {
        let creator_set = curve.creator != Pubkey::default();
        match side {
            Side::Buy => {
                let total_bps = fees.protocol_bps + if creator_set { fees.creator_bps } else { 0 };
                let sol_into_curve = (amount.saturating_sub(1) as u128 * 10_000
                    / (total_bps as u128 + 10_000)) as u64;
                let expected_out = base_out_for_quote_in(
                    curve.quote_reserves,
                    curve.base_reserves,
                    sol_into_curve,
                )
                .min(curve.real_token_reserves);

                Quote {
                    in_amount: amount,
                    expected_out,
                    min_out: slippage_down(expected_out, slippage_bps),
                    fee: amount.saturating_sub(sol_into_curve),
                }
            }
            Side::Sell => {
                let gross_sol_out =
                    quote_out_for_base_in(curve.quote_reserves, curve.base_reserves, amount);
                let fee = fee_ceil(gross_sol_out, fees.protocol_bps)
                    + if creator_set {
                        fee_ceil(gross_sol_out, fees.creator_bps)
                    } else {
                        0
                    };
                let expected_out = gross_sol_out.saturating_sub(fee);
                Quote {
                    in_amount: amount,
                    expected_out,
                    min_out: slippage_down(expected_out, slippage_bps),
                    fee,
                }
            }
        }
    }

    fn buy_ix(
        &self,
        p: &Trade,
        pool: Pubkey,
        curve: &Curve,
        fee_recipient: Pubkey,
        spendable_sol: u64,
        min_tokens: u64,
    ) -> Instruction {
        let mut data = Vec::with_capacity(25);
        data.extend_from_slice(&anchor_discriminator(BUY_IX));
        data.extend_from_slice(&spendable_sol.to_le_bytes());
        data.extend_from_slice(&min_tokens.to_le_bytes());
        data.push(1);
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(Self::global_pda(), false),
                AccountMeta::new(fee_recipient, false),
                AccountMeta::new_readonly(p.mint, false),
                AccountMeta::new(pool, false),
                AccountMeta::new(ata(&pool, &p.mint, &curve.token_program), false),
                AccountMeta::new(ata(&p.wallet, &p.mint, &curve.token_program), false),
                AccountMeta::new(p.wallet, true),
                AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
                AccountMeta::new_readonly(curve.token_program, false),
                AccountMeta::new(Self::creator_vault_pda(&curve.creator), false),
                AccountMeta::new_readonly(Self::event_authority_pda(), false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(Self::global_volume_pda(), false),
                AccountMeta::new(Self::user_volume_pda(&p.wallet), false),
                AccountMeta::new_readonly(Self::fee_config_pda(), false),
                AccountMeta::new_readonly(FEE_PROGRAM_ID, false),
                AccountMeta::new_readonly(Self::bonding_curve_v2_pda(&p.mint), false),
                AccountMeta::new(BUYBACK_RECIPIENT, false),
            ],
            data,
        }
    }

    fn sell_ix(
        &self,
        p: &Trade,
        pool: Pubkey,
        curve: &Curve,
        fee_recipient: Pubkey,
        base_in: u64,
        min_sol: u64,
    ) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&anchor_discriminator(SELL_IX));
        data.extend_from_slice(&base_in.to_le_bytes());
        data.extend_from_slice(&min_sol.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(Self::global_pda(), false),
                AccountMeta::new(fee_recipient, false),
                AccountMeta::new_readonly(p.mint, false),
                AccountMeta::new(pool, false),
                AccountMeta::new(ata(&pool, &p.mint, &curve.token_program), false),
                AccountMeta::new(ata(&p.wallet, &p.mint, &curve.token_program), false),
                AccountMeta::new(p.wallet, true),
                AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
                AccountMeta::new(Self::creator_vault_pda(&curve.creator), false),
                AccountMeta::new_readonly(curve.token_program, false),
                AccountMeta::new_readonly(Self::event_authority_pda(), false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(Self::fee_config_pda(), false),
                AccountMeta::new_readonly(FEE_PROGRAM_ID, false),
                AccountMeta::new_readonly(Self::bonding_curve_v2_pda(&p.mint), false),
                AccountMeta::new(BUYBACK_RECIPIENT, false),
            ],
            data,
        }
    }
}

#[async_trait]
impl Dex for PumpFun {
    fn name(&self) -> &'static str {
        "pumpfun"
    }

    async fn quote(&self, p: &Trade) -> Result<Quote> {
        let pool = p.pool.unwrap_or_else(|| Self::bonding_curve_pda(&p.mint));
        let (curve, fees) = (
            self.load_curve(&pool, &p.mint).await?,
            self.fee_settings().await?,
        );
        Ok(self.compute_quote(&curve, &fees, p.side, p.amount, p.slippage_bps))
    }

    async fn swap(&self, p: &Trade) -> Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
        let pool = p.pool.unwrap_or_else(|| Self::bonding_curve_pda(&p.mint));
        let (curve, fees) = (
            self.load_curve(&pool, &p.mint).await?,
            self.fee_settings().await?,
        );
        let quote = self.compute_quote(&curve, &fees, p.side, p.amount, p.slippage_bps);
        let instructions = match p.side {
            Side::Buy => vec![
                create_ata_idempotent(&p.wallet, &p.wallet, &p.mint, &curve.token_program),
                self.buy_ix(p, pool, &curve, fees.recipient, p.amount, quote.min_out),
            ],
            Side::Sell => {
                vec![self.sell_ix(p, pool, &curve, fees.recipient, p.amount, quote.min_out)]
            }
        };
        Ok((instructions, vec![]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Venue;

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
        let mint: Pubkey = "9JihXt4NZtZzURoMm1KrGN6y2a9LH9xdKkh5p9kJpump"
            .parse()
            .unwrap();
        let pool = PumpFun::bonding_curve_pda(&mint);

        let wallet = dex.load_curve(&pool, &mint).await.unwrap().creator;

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
}
