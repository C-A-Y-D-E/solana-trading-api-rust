use std::sync::{Arc, OnceLock};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use borsh::BorshDeserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{Pubkey, pubkey};

use crate::dexes::common::*;
use crate::types::{Dex, PreparedSwap, Quote, Settlement, Side, Trade};

pub const PROGRAM_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
pub const FEE_PROGRAM_ID: Pubkey = pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

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
    is_mayhem_mode: bool,
    is_cashback_coin: bool,
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
    fee_recipients: [Pubkey; 7],
    set_creator_authority: Pubkey,
    admin_set_creator_authority: Pubkey,
    create_v2_enabled: bool,
    whitelist_pda: Pubkey,
    reserved_fee_recipient: Pubkey,
    mayhem_mode_enabled: bool,
    reserved_fee_recipients: [Pubkey; 7],
    is_cashback_enabled: bool,
    buyback_fee_recipients: [Pubkey; 8],
}

#[derive(Clone, Copy)]
struct FeeSettings {
    standard_recipient: Pubkey,
    mayhem_recipient: Pubkey,
    buyback_recipient: Pubkey,
    protocol_bps: u64,
    creator_bps: u64,
}

impl FeeSettings {
    fn recipient(self, is_mayhem: bool) -> Pubkey {
        if is_mayhem {
            self.mayhem_recipient
        } else {
            self.standard_recipient
        }
    }
}

struct Curve {
    creator: Pubkey,
    base_reserves: u64,
    quote_reserves: u64,
    real_token_reserves: u64,
    is_mayhem: bool,
    is_cashback: bool,
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

    /// Shared accounts for any token and trading wallet, using current fee recipients.
    pub async fn shared_lookup_addresses(&self) -> Result<Vec<Pubkey>> {
        let account = self.rpc.get_account(&Self::global_pda()).await?;
        let global: GlobalAccount = decode_account(&account.data)?;
        let mut addresses = vec![
            PROGRAM_ID,
            FEE_PROGRAM_ID,
            Self::global_pda(),
            Self::event_authority_pda(),
            Self::global_volume_pda(),
            Self::fee_config_pda(),
            global.fee_recipient,
            global.reserved_fee_recipient,
        ];
        addresses.extend(global.fee_recipients);
        addresses.extend(global.reserved_fee_recipients);
        addresses.extend(global.buyback_fee_recipients);
        Ok(addresses)
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
            standard_recipient: global.fee_recipient,
            mayhem_recipient: global.reserved_fee_recipient,
            buyback_recipient: global.buyback_fee_recipients[0],
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
            is_mayhem: c.is_mayhem_mode,
            is_cashback: c.is_cashback_coin,
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
                let curve_output = base_out_for_quote_in(
                    curve.quote_reserves,
                    curve.base_reserves,
                    sol_into_curve,
                );
                let expected_out = curve_output.min(curve.real_token_reserves);

                Quote {
                    in_amount: amount,
                    price_impact_bps: if curve_output > curve.real_token_reserves {
                        None
                    } else {
                        crate::price_impact::exact_input(
                            curve.quote_reserves,
                            curve.base_reserves,
                            sol_into_curve,
                        )
                    },
                    application_fee: 0,
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
                    price_impact_bps: crate::price_impact::exact_input(
                        curve.base_reserves,
                        curve.quote_reserves,
                        amount,
                    ),
                    application_fee: 0,
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
        fees: FeeSettings,
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
                AccountMeta::new(fees.recipient(curve.is_mayhem), false),
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
                AccountMeta::new(fees.buyback_recipient, false),
            ],
            data,
        }
    }

    fn sell_ix(
        &self,
        p: &Trade,
        pool: Pubkey,
        curve: &Curve,
        fees: FeeSettings,
        base_in: u64,
        min_sol: u64,
    ) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&anchor_discriminator(SELL_IX));
        data.extend_from_slice(&base_in.to_le_bytes());
        data.extend_from_slice(&min_sol.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new_readonly(Self::global_pda(), false),
            AccountMeta::new(fees.recipient(curve.is_mayhem), false),
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
        ];
        // Cashback coins require the user volume accumulator at remaining_accounts[0],
        // before bonding_curve_v2 — anything else there fails with 6073
        // (InvalidCashbackAccumulator).
        if curve.is_cashback {
            accounts.push(AccountMeta::new(Self::user_volume_pda(&p.wallet), false));
        }
        accounts.push(AccountMeta::new_readonly(
            Self::bonding_curve_v2_pda(&p.mint),
            false,
        ));
        accounts.push(AccountMeta::new(fees.buyback_recipient, false));
        Instruction {
            program_id: PROGRAM_ID,
            accounts,
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
        anyhow::ensure!(
            p.settlement == Settlement::Sol,
            "use TradingClient to bridge USDC for Pump.fun"
        );
        let pool = p.pool.unwrap_or_else(|| Self::bonding_curve_pda(&p.mint));
        let (curve, fees) = (
            self.load_curve(&pool, &p.mint).await?,
            self.fee_settings().await?,
        );
        Ok(self.compute_quote(&curve, &fees, p.side, p.amount, p.slippage_bps))
    }

    async fn prepare_swap(&self, p: &Trade) -> Result<PreparedSwap> {
        anyhow::ensure!(
            p.settlement == Settlement::Sol,
            "use TradingClient to bridge USDC for Pump.fun"
        );
        let pool = p.pool.unwrap_or_else(|| Self::bonding_curve_pda(&p.mint));
        let (curve, fees) = (
            self.load_curve(&pool, &p.mint).await?,
            self.fee_settings().await?,
        );
        let quote = self.compute_quote(&curve, &fees, p.side, p.amount, p.slippage_bps);
        let instructions = match p.side {
            Side::Buy => vec![
                create_ata_idempotent(&p.wallet, &p.wallet, &p.mint, &curve.token_program),
                self.buy_ix(p, pool, &curve, fees, p.amount, quote.min_out),
            ],
            Side::Sell => {
                vec![self.sell_ix(p, pool, &curve, fees, p.amount, quote.min_out)]
            }
        };
        Ok(PreparedSwap {
            venue: self.name(),
            quote,
            instructions,
            lookup_tables: vec![],
        })
    }
}

#[cfg(test)]
mod tests;
