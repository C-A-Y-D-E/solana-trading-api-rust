use std::sync::{Arc, OnceLock};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use borsh::BorshDeserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{Pubkey, pubkey};

use crate::dexes::common::*;
use crate::types::{Dex, PreparedSwap, Quote, Settlement, Side, Trade};

pub const PROGRAM_ID: Pubkey = pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
pub const FEE_PROGRAM_ID: Pubkey = pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const DEFAULT_SOL_USDC_POOL: Pubkey = pubkey!("Gf7sXMoP8iRw4iiXmJ1nq4vxcRycbGXy5RL8a8LnTd3v");

const PUMPFUN_PROGRAM_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");

pub(crate) const BUY_EXACT_BASE_OUT_IX: &str = "buy";
pub(crate) const BUY_EXACT_QUOTE_IN_IX: &str = "buy_exact_quote_in";
pub(crate) const SELL_IX: &str = "sell";
const MAX_BRIDGE_SLIPPAGE_BPS: u64 = 50;

#[derive(BorshDeserialize)]
#[cfg_attr(test, derive(borsh::BorshSerialize))]
#[allow(dead_code)]
struct PoolAccount {
    discriminator: u64,
    pool_bump: u8,
    index: u16,
    creator: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    lp_mint: Pubkey,
    pool_base_token_account: Pubkey,
    pool_quote_token_account: Pubkey,
    lp_supply: u64,
    coin_creator: Pubkey,
    is_mayhem_mode: bool,
    is_cashback_coin: bool,
}

fn decode_pool(data: &[u8]) -> Result<(PoolAccount, i128)> {
    let mut remaining = data;
    let pool = PoolAccount::deserialize_reader(&mut remaining)?;
    // Older accounts predate the appended virtual quote reserve field.
    let virtual_quote_reserves = if remaining.is_empty() {
        0
    } else {
        i128::deserialize_reader(&mut remaining)?
    };
    Ok((pool, virtual_quote_reserves))
}

fn effective_quote_reserves(vault_balance: u64, virtual_reserves: i128) -> Result<u64> {
    let reserves = i128::from(vault_balance)
        .checked_add(virtual_reserves)
        .ok_or_else(|| anyhow!("pumpswap: effective quote reserves overflow"))?;
    u64::try_from(reserves)
        .map_err(|_| anyhow!("pumpswap: effective quote reserves outside u64 range"))
}

#[derive(BorshDeserialize)]
#[allow(dead_code)]
struct GlobalConfigAccount {
    discriminator: u64,
    admin: Pubkey,
    lp_fee_basis_points: u64,
    protocol_fee_basis_points: u64,
    disable_flags: u8,
    protocol_fee_recipients: [Pubkey; 8],
    coin_creator_fee_basis_points: u64,
    admin_set_coin_creator_authority: Pubkey,
    whitelist_pda: Pubkey,
    reserved_fee_recipient: Pubkey,
    mayhem_mode_enabled: bool,
    reserved_fee_recipients: [Pubkey; 7],
    is_cashback_enabled: bool,
    buyback_fee_recipients: [Pubkey; 8],
}

#[derive(Clone, Copy)]
struct FeeSettings {
    standard_protocol_recipient: Pubkey,
    mayhem_protocol_recipient: Pubkey,
    buyback_recipient: Pubkey,
    lp_bps: u64,
    protocol_bps: u64,
    creator_bps: u64,
}

impl FeeSettings {
    fn protocol_recipient(self, is_mayhem: bool) -> Pubkey {
        if is_mayhem {
            self.mayhem_protocol_recipient
        } else {
            self.standard_protocol_recipient
        }
    }
}

struct PoolState {
    coin_creator: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    base_reserves: u64,
    quote_reserves: u64,
    is_mayhem: bool,
    is_cashback: bool,

    base_token_program: Pubkey,
    quote_token_program: Pubkey,
}

enum BuyAmounts {
    ExactBaseOut { base_out: u64, max_quote_in: u64 },
    ExactQuoteIn { quote_in: u64, min_base_out: u64 },
}

#[derive(Debug, PartialEq, Eq)]
enum SwapRoute {
    DirectSol,
    ViaUsdc,
}

pub struct PumpSwap {
    rpc: Arc<RpcClient>,
    fee_settings: OnceLock<FeeSettings>,
    sol_usdc_pool: Pubkey,
}

impl PumpSwap {
    pub(crate) fn sol_usdc_pool(&self) -> Pubkey {
        self.sol_usdc_pool
    }

    pub(crate) async fn quote_mint(&self, trade: &Trade) -> Result<Pubkey> {
        let address = trade
            .pool
            .unwrap_or_else(|| Self::canonical_pool_pda(&trade.mint));
        let account = self.rpc.get_account(&address).await?;
        let (pool, _) = decode_pool(&account.data)?;
        anyhow::ensure!(
            account.owner == PROGRAM_ID && pool.base_mint == trade.mint,
            "invalid PumpSwap pool"
        );
        Ok(pool.quote_mint)
    }

    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self {
            rpc,
            fee_settings: OnceLock::new(),
            sol_usdc_pool: DEFAULT_SOL_USDC_POOL,
        }
    }

    pub fn with_sol_usdc_pool(mut self, pool: Pubkey) -> Self {
        self.sol_usdc_pool = pool;
        self
    }

    /// Shared venue accounts plus the fixed SOL/USDC bridge, without target or user accounts.
    pub async fn shared_lookup_addresses(&self) -> Result<Vec<Pubkey>> {
        let account = self.rpc.get_account(&Self::global_config_pda()).await?;
        let global: GlobalConfigAccount = decode_account(&account.data)?;
        let mut addresses = vec![
            PROGRAM_ID,
            FEE_PROGRAM_ID,
            Self::global_config_pda(),
            Self::event_authority_pda(),
            Self::global_volume_pda(),
            Self::fee_config_pda(),
        ];
        let recipients = global
            .protocol_fee_recipients
            .into_iter()
            .chain([global.reserved_fee_recipient])
            .chain(global.reserved_fee_recipients)
            .chain(global.buyback_fee_recipients);
        for recipient in recipients.filter(|key| *key != Pubkey::default()) {
            addresses.push(recipient);
            addresses.push(ata(&recipient, &WSOL, &TOKEN_PROGRAM));
            addresses.push(ata(&recipient, &USDC_MINT, &TOKEN_PROGRAM));
        }
        addresses.extend(self.bridge_lookup_addresses().await?);
        Ok(addresses)
    }

    async fn bridge_lookup_addresses(&self) -> Result<Vec<Pubkey>> {
        let pool = self.load_sol_usdc_pool().await?;
        let creator_vault = Self::coin_creator_vault_authority_pda(&pool.coin_creator);
        Ok(vec![
            self.sol_usdc_pool,
            pool.base_mint,
            pool.quote_mint,
            pool.base_vault,
            pool.quote_vault,
            creator_vault,
            ata(&creator_vault, &pool.quote_mint, &pool.quote_token_program),
            Self::pool_v2_pda(&pool.base_mint),
        ])
    }

    pub fn canonical_pool_pda(mint: &Pubkey) -> Pubkey {
        let pool_authority = pda(&[b"pool-authority", mint.as_ref()], &PUMPFUN_PROGRAM_ID);
        pda(
            &[
                b"pool",
                &0u16.to_le_bytes(),
                pool_authority.as_ref(),
                mint.as_ref(),
                WSOL.as_ref(),
            ],
            &PROGRAM_ID,
        )
    }

    fn global_config_pda() -> Pubkey {
        pda(&[b"global_config"], &PROGRAM_ID)
    }
    fn event_authority_pda() -> Pubkey {
        pda(&[b"__event_authority"], &PROGRAM_ID)
    }
    fn coin_creator_vault_authority_pda(coin_creator: &Pubkey) -> Pubkey {
        pda(&[b"creator_vault", coin_creator.as_ref()], &PROGRAM_ID)
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
    fn pool_v2_pda(base_mint: &Pubkey) -> Pubkey {
        pda(&[b"pool-v2", base_mint.as_ref()], &PROGRAM_ID)
    }

    async fn fee_settings(&self) -> Result<FeeSettings> {
        if let Some(f) = self.fee_settings.get() {
            return Ok(*f);
        }
        let acc = self.rpc.get_account(&Self::global_config_pda()).await?;
        let gc: GlobalConfigAccount = decode_account(&acc.data)?;
        let f = FeeSettings {
            standard_protocol_recipient: gc.protocol_fee_recipients[0],
            mayhem_protocol_recipient: gc.reserved_fee_recipient,
            buyback_recipient: gc.buyback_fee_recipients[0],
            lp_bps: gc.lp_fee_basis_points,
            protocol_bps: gc.protocol_fee_basis_points,
            creator_bps: gc.coin_creator_fee_basis_points,
        };
        let _ = self.fee_settings.set(f);
        Ok(f)
    }

    async fn load_pool(&self, pool_address: &Pubkey, mint: &Pubkey) -> Result<PoolState> {
        let acc = self.rpc.get_account(pool_address).await?;
        let (pool, virtual_quote_reserves) = decode_pool(&acc.data)
            .map_err(|e| anyhow!("pumpswap: bad pool {pool_address}: {e}"))?;
        if pool.base_mint != *mint {
            return Err(anyhow!(
                "pumpswap: pool base mint {} != requested mint {mint}",
                pool.base_mint
            ));
        }

        let mints = self
            .rpc
            .get_multiple_accounts(&[pool.base_mint, pool.quote_mint])
            .await?;
        let base_token_program = mints[0]
            .as_ref()
            .ok_or_else(|| anyhow!("pumpswap: base mint not found"))?
            .owner;
        let quote_token_program = mints[1]
            .as_ref()
            .ok_or_else(|| anyhow!("pumpswap: quote mint not found"))?
            .owner;
        let (base_balance, quote_balance) = tokio::try_join!(
            self.rpc
                .get_token_account_balance(&pool.pool_base_token_account),
            self.rpc
                .get_token_account_balance(&pool.pool_quote_token_account),
        )?;
        Ok(PoolState {
            coin_creator: pool.coin_creator,
            base_mint: pool.base_mint,
            quote_mint: pool.quote_mint,
            base_vault: pool.pool_base_token_account,
            quote_vault: pool.pool_quote_token_account,
            base_reserves: base_balance.amount.parse()?,
            quote_reserves: effective_quote_reserves(
                quote_balance.amount.parse()?,
                virtual_quote_reserves,
            )?,
            is_mayhem: pool.is_mayhem_mode,
            is_cashback: pool.is_cashback_coin,
            base_token_program,
            quote_token_program,
        })
    }

    fn compute_quote(
        &self,
        pool: &PoolState,
        fees: &FeeSettings,
        side: Side,
        amount: u64,
        slippage_bps: u64,
    ) -> Quote {
        let creator_bps = if pool.coin_creator != Pubkey::default() {
            fees.creator_bps
        } else {
            0
        };
        match side {
            Side::Buy => {
                let total_bps = fees.lp_bps + fees.protocol_bps + creator_bps;
                let mut quote_into_pool =
                    (amount as u128 * 10_000 / (10_000 + total_bps as u128)) as u64;
                let fee = fee_floor(quote_into_pool, fees.lp_bps)
                    + fee_floor(quote_into_pool, fees.protocol_bps)
                    + fee_floor(quote_into_pool, creator_bps);
                if quote_into_pool + fee > amount {
                    quote_into_pool =
                        quote_into_pool.saturating_sub(quote_into_pool + fee - amount);
                }
                let expected_out = base_out_for_quote_in(
                    pool.quote_reserves,
                    pool.base_reserves,
                    quote_into_pool.saturating_sub(1),
                );

                Quote {
                    in_amount: amount,
                    price_impact_bps: crate::price_impact::exact_input(
                        pool.quote_reserves,
                        pool.base_reserves,
                        quote_into_pool.saturating_sub(1),
                    ),
                    application_fee: 0,
                    sponsorship_fee: 0,
                    expected_out,
                    min_out: slippage_down(expected_out, slippage_bps),
                    fee: amount.saturating_sub(quote_into_pool),
                }
            }
            Side::Sell => {
                let gross_quote_out =
                    quote_out_for_base_in(pool.quote_reserves, pool.base_reserves, amount);
                let fee = fee_floor(gross_quote_out, fees.lp_bps)
                    + fee_floor(gross_quote_out, fees.protocol_bps)
                    + fee_floor(gross_quote_out, creator_bps);
                let expected_out = gross_quote_out.saturating_sub(fee);
                Quote {
                    in_amount: amount,
                    price_impact_bps: crate::price_impact::exact_input(
                        pool.base_reserves,
                        pool.quote_reserves,
                        amount,
                    ),
                    application_fee: 0,
                    sponsorship_fee: 0,
                    expected_out,
                    min_out: slippage_down(expected_out, slippage_bps),
                    fee,
                }
            }
        }
    }

    fn named_accounts(
        &self,
        p: &Trade,
        pool_address: Pubkey,
        pool: &PoolState,
        fees: FeeSettings,
    ) -> Vec<AccountMeta> {
        let creator_vault_authority = Self::coin_creator_vault_authority_pda(&pool.coin_creator);
        let protocol_recipient = fees.protocol_recipient(pool.is_mayhem);
        vec![
            AccountMeta::new(pool_address, false),
            AccountMeta::new(p.wallet, true),
            AccountMeta::new_readonly(Self::global_config_pda(), false),
            AccountMeta::new_readonly(pool.base_mint, false),
            AccountMeta::new_readonly(pool.quote_mint, false),
            AccountMeta::new(
                ata(&p.wallet, &pool.base_mint, &pool.base_token_program),
                false,
            ),
            AccountMeta::new(
                ata(&p.wallet, &pool.quote_mint, &pool.quote_token_program),
                false,
            ),
            AccountMeta::new(pool.base_vault, false),
            AccountMeta::new(pool.quote_vault, false),
            AccountMeta::new_readonly(protocol_recipient, false),
            AccountMeta::new(
                ata(
                    &protocol_recipient,
                    &pool.quote_mint,
                    &pool.quote_token_program,
                ),
                false,
            ),
            AccountMeta::new_readonly(pool.base_token_program, false),
            AccountMeta::new_readonly(pool.quote_token_program, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(ATA_PROGRAM, false),
            AccountMeta::new_readonly(Self::event_authority_pda(), false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new(
                ata(
                    &creator_vault_authority,
                    &pool.quote_mint,
                    &pool.quote_token_program,
                ),
                false,
            ),
            AccountMeta::new_readonly(creator_vault_authority, false),
        ]
    }

    fn remaining_accounts(
        &self,
        p: &Trade,
        pool: &PoolState,
        fees: &FeeSettings,
        side: Side,
    ) -> Vec<AccountMeta> {
        let mut accounts = vec![];
        if pool.is_cashback {
            accounts.push(AccountMeta::new(
                ata(
                    &Self::user_volume_pda(&p.wallet),
                    &pool.quote_mint,
                    &pool.quote_token_program,
                ),
                false,
            ));
            if side == Side::Sell {
                accounts.push(AccountMeta::new(Self::user_volume_pda(&p.wallet), false));
            }
        }
        if pool.coin_creator != Pubkey::default() {
            accounts.push(AccountMeta::new_readonly(
                Self::pool_v2_pda(&pool.base_mint),
                false,
            ));
        }
        accounts.push(AccountMeta::new_readonly(fees.buyback_recipient, false));
        accounts.push(AccountMeta::new(
            ata(
                &fees.buyback_recipient,
                &pool.quote_mint,
                &pool.quote_token_program,
            ),
            false,
        ));
        accounts
    }

    fn swap_accounts(
        &self,
        p: &Trade,
        pool_address: Pubkey,
        pool: &PoolState,
        fees: FeeSettings,
    ) -> Vec<AccountMeta> {
        let mut accounts = self.named_accounts(p, pool_address, pool, fees);
        if p.side == Side::Buy {
            accounts.push(AccountMeta::new_readonly(Self::global_volume_pda(), false));
            accounts.push(AccountMeta::new(Self::user_volume_pda(&p.wallet), false));
        }
        accounts.push(AccountMeta::new_readonly(Self::fee_config_pda(), false));
        accounts.push(AccountMeta::new_readonly(FEE_PROGRAM_ID, false));
        accounts.extend(self.remaining_accounts(p, pool, &fees, p.side));
        accounts
    }

    fn buy_instructions(
        &self,
        p: &Trade,
        pool_address: Pubkey,
        pool: &PoolState,
        fees: FeeSettings,
        amounts: BuyAmounts,
    ) -> Vec<Instruction> {
        let mut data = Vec::with_capacity(25);
        let quote_to_wrap = match amounts {
            BuyAmounts::ExactBaseOut {
                base_out,
                max_quote_in,
            } => {
                data.extend_from_slice(&anchor_discriminator(BUY_EXACT_BASE_OUT_IX));
                data.extend_from_slice(&base_out.to_le_bytes());
                data.extend_from_slice(&max_quote_in.to_le_bytes());
                max_quote_in
            }
            BuyAmounts::ExactQuoteIn {
                quote_in,
                min_base_out,
            } => {
                data.extend_from_slice(&anchor_discriminator(BUY_EXACT_QUOTE_IN_IX));
                data.extend_from_slice(&quote_in.to_le_bytes());
                data.extend_from_slice(&min_base_out.to_le_bytes());
                quote_in
            }
        };
        data.push(1);

        let user_quote_ata = ata(&p.wallet, &pool.quote_mint, &pool.quote_token_program);
        let mut instructions = vec![
            create_ata_idempotent(
                &p.wallet,
                &p.wallet,
                &pool.base_mint,
                &pool.base_token_program,
            ),
            create_ata_idempotent(
                &p.wallet,
                &p.wallet,
                &pool.quote_mint,
                &pool.quote_token_program,
            ),
        ];
        let wrap_sol = pool.quote_mint == WSOL;
        if wrap_sol {
            instructions.push(system_transfer(&p.wallet, &user_quote_ata, quote_to_wrap));
            instructions.push(sync_native(&user_quote_ata));
        }
        instructions.push(Instruction {
            program_id: PROGRAM_ID,
            accounts: self.swap_accounts(p, pool_address, pool, fees),
            data,
        });
        if wrap_sol {
            instructions.push(close_account(&user_quote_ata, &p.wallet, &p.wallet));
        }
        instructions
    }

    fn sell_instructions(
        &self,
        trade: &Trade,
        pool_address: Pubkey,
        pool: &PoolState,
        fees: FeeSettings,
        min_quote_out: u64,
    ) -> Vec<Instruction> {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&anchor_discriminator(SELL_IX));
        data.extend_from_slice(&trade.amount.to_le_bytes());
        data.extend_from_slice(&min_quote_out.to_le_bytes());
        let mut instructions = vec![
            create_ata_idempotent(
                &trade.wallet,
                &trade.wallet,
                &pool.quote_mint,
                &pool.quote_token_program,
            ),
            Instruction {
                program_id: PROGRAM_ID,
                accounts: self.swap_accounts(trade, pool_address, pool, fees),
                data,
            },
        ];
        if pool.quote_mint == WSOL {
            let quote_ata = ata(&trade.wallet, &WSOL, &pool.quote_token_program);
            instructions.push(close_account(&quote_ata, &trade.wallet, &trade.wallet));
        }
        instructions
    }

    pub(crate) fn route_slippage(total_bps: u64) -> (u64, u64) {
        let bridge_bps = (total_bps / 2).min(MAX_BRIDGE_SLIPPAGE_BPS);
        (bridge_bps, total_bps - bridge_bps)
    }

    async fn load_sol_usdc_pool(&self) -> Result<PoolState> {
        let bridge_pool = self.load_pool(&self.sol_usdc_pool, &USDC_MINT).await?;
        if bridge_pool.quote_mint != WSOL {
            return Err(anyhow!(
                "pumpswap bridge pool {} must be USDC/WSOL",
                self.sol_usdc_pool
            ));
        }
        Ok(bridge_pool)
    }

    fn swap_route(pool: &PoolState) -> Result<SwapRoute> {
        match pool.quote_mint {
            WSOL => Ok(SwapRoute::DirectSol),
            USDC_MINT => Ok(SwapRoute::ViaUsdc),
            quote_mint => Err(anyhow!("pumpswap does not support quote mint {quote_mint}")),
        }
    }

    fn route_quotes(
        &self,
        p: &Trade,
        bridge_pool: &PoolState,
        target_pool: &PoolState,
        fees: &FeeSettings,
    ) -> (Quote, Quote) {
        let (bridge_slippage, target_slippage) = Self::route_slippage(p.slippage_bps);
        if p.side == Side::Sell {
            let target_quote =
                self.compute_quote(target_pool, fees, Side::Sell, p.amount, target_slippage);
            // Only spend guaranteed proceeds; favorable execution leaves surplus USDC in the wallet.
            let bridge_quote = self.compute_quote(
                bridge_pool,
                fees,
                Side::Sell,
                target_quote.min_out,
                bridge_slippage,
            );
            return (bridge_quote, target_quote);
        }
        let mut bridge_quote =
            self.compute_quote(bridge_pool, fees, Side::Buy, p.amount, bridge_slippage);
        // This leg buys exactly min_out; max input is a spending cap, not its execution size.
        bridge_quote.price_impact_bps =
            crate::price_impact::exact_output(bridge_pool.base_reserves, bridge_quote.min_out);
        let target_quote = self.compute_quote(
            target_pool,
            fees,
            Side::Buy,
            bridge_quote.min_out,
            target_slippage,
        );
        (bridge_quote, target_quote)
    }

    async fn quote_via_usdc(
        &self,
        p: &Trade,
        target_pool: &PoolState,
        fees: &FeeSettings,
    ) -> Result<Quote> {
        let bridge_pool = self.load_sol_usdc_pool().await?;
        let (bridge_quote, target_quote) = self.route_quotes(p, &bridge_pool, target_pool, fees);
        let output_quote = match p.side {
            Side::Buy => target_quote,
            Side::Sell => bridge_quote,
        };
        Ok(Quote {
            in_amount: p.amount,
            price_impact_bps: crate::price_impact::combine(
                bridge_quote.price_impact_bps,
                target_quote.price_impact_bps,
            ),
            application_fee: 0,
            sponsorship_fee: 0,
            expected_out: output_quote.expected_out,
            min_out: output_quote.min_out,
            fee: bridge_quote.fee,
        })
    }

    fn sell_via_usdc_instructions(
        &self,
        trade: &Trade,
        target_pool_address: Pubkey,
        target_pool: &PoolState,
        bridge_pool: &PoolState,
        fees: FeeSettings,
    ) -> Result<Vec<Instruction>> {
        let (bridge_quote, target_quote) =
            self.route_quotes(trade, bridge_pool, target_pool, &fees);
        if target_quote.min_out == 0 || bridge_quote.min_out == 0 {
            return Err(anyhow!("pumpswap USDC route output is zero"));
        }
        let mut instructions = self.sell_instructions(
            trade,
            target_pool_address,
            target_pool,
            fees,
            target_quote.min_out,
        );
        let mut bridge_trade = *trade;
        bridge_trade.mint = USDC_MINT;
        bridge_trade.pool = Some(self.sol_usdc_pool);
        bridge_trade.amount = target_quote.min_out;
        instructions.extend(self.sell_instructions(
            &bridge_trade,
            self.sol_usdc_pool,
            bridge_pool,
            fees,
            bridge_quote.min_out,
        ));
        Ok(instructions)
    }

    async fn swap_via_usdc(
        &self,
        p: &Trade,
        target_pool_address: Pubkey,
        target_pool: &PoolState,
        fees: FeeSettings,
    ) -> Result<PreparedSwap> {
        let bridge_pool = self.load_sol_usdc_pool().await?;
        self.prepare_usdc_route(p, target_pool_address, target_pool, &bridge_pool, fees)
    }

    fn prepare_usdc_route(
        &self,
        p: &Trade,
        target_pool_address: Pubkey,
        target_pool: &PoolState,
        bridge_pool: &PoolState,
        fees: FeeSettings,
    ) -> Result<PreparedSwap> {
        let (bridge_quote, target_quote) = self.route_quotes(p, bridge_pool, target_pool, &fees);
        let output_quote = if p.side == Side::Sell {
            bridge_quote
        } else {
            target_quote
        };
        let quote = Quote {
            in_amount: p.amount,
            price_impact_bps: crate::price_impact::combine(
                bridge_quote.price_impact_bps,
                target_quote.price_impact_bps,
            ),
            fee: bridge_quote.fee,
            ..output_quote
        };
        if p.side == Side::Sell {
            return Ok(PreparedSwap {
                venue: self.name(),
                quote,
                instructions: self.sell_via_usdc_instructions(
                    p,
                    target_pool_address,
                    target_pool,
                    bridge_pool,
                    fees,
                )?,
                lookup_tables: vec![],
            });
        }
        if bridge_quote.min_out == 0 || target_quote.min_out == 0 {
            return Err(anyhow!("pumpswap USDC route output is zero"));
        }

        let mut bridge_trade = *p;
        bridge_trade.mint = USDC_MINT;
        bridge_trade.pool = Some(self.sol_usdc_pool);
        let mut instructions = self.buy_instructions(
            &bridge_trade,
            self.sol_usdc_pool,
            bridge_pool,
            fees,
            BuyAmounts::ExactBaseOut {
                base_out: bridge_quote.min_out,
                max_quote_in: p.amount,
            },
        );

        let mut target_trade = *p;
        target_trade.amount = bridge_quote.min_out;
        target_trade.slippage_bps = Self::route_slippage(p.slippage_bps).1;
        instructions.extend(self.buy_instructions(
            &target_trade,
            target_pool_address,
            target_pool,
            fees,
            BuyAmounts::ExactQuoteIn {
                quote_in: bridge_quote.min_out,
                min_base_out: target_quote.min_out,
            },
        ));
        Ok(PreparedSwap {
            venue: self.name(),
            quote,
            instructions,
            lookup_tables: vec![],
        })
    }
}

#[async_trait]
impl Dex for PumpSwap {
    fn name(&self) -> &'static str {
        "pumpswap"
    }

    async fn quote(&self, p: &Trade) -> Result<Quote> {
        let pool_address = p.pool.unwrap_or_else(|| Self::canonical_pool_pda(&p.mint));
        let (pool, fees) = (
            self.load_pool(&pool_address, &p.mint).await?,
            self.fee_settings().await?,
        );
        if p.settlement == Settlement::Usdc {
            anyhow::ensure!(
                pool.quote_mint == USDC_MINT,
                "use TradingClient for USDC settlement through a SOL pool"
            );
        } else if Self::swap_route(&pool)? == SwapRoute::ViaUsdc {
            return self.quote_via_usdc(p, &pool, &fees).await;
        }
        Ok(self.compute_quote(&pool, &fees, p.side, p.amount, p.slippage_bps))
    }

    async fn prepare_swap(&self, p: &Trade) -> Result<PreparedSwap> {
        let pool_address = p.pool.unwrap_or_else(|| Self::canonical_pool_pda(&p.mint));
        let (pool, fees) = (
            self.load_pool(&pool_address, &p.mint).await?,
            self.fee_settings().await?,
        );
        if p.settlement == Settlement::Usdc {
            anyhow::ensure!(
                pool.quote_mint == USDC_MINT,
                "use TradingClient for USDC settlement through a SOL pool"
            );
        } else if Self::swap_route(&pool)? == SwapRoute::ViaUsdc {
            return self.swap_via_usdc(p, pool_address, &pool, fees).await;
        }
        let quote = self.compute_quote(&pool, &fees, p.side, p.amount, p.slippage_bps);
        let instructions = match p.side {
            Side::Buy => self.buy_instructions(
                p,
                pool_address,
                &pool,
                fees,
                BuyAmounts::ExactQuoteIn {
                    quote_in: p.amount,
                    min_base_out: quote.min_out,
                },
            ),
            Side::Sell => self.sell_instructions(p, pool_address, &pool, fees, quote.min_out),
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
#[path = "../../../tests/unit/dexes/pumpswap/mod.rs"]
mod tests;
