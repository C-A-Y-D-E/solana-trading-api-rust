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

pub const PROGRAM_ID: Pubkey = pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
pub const FEE_PROGRAM_ID: Pubkey = pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const DEFAULT_SOL_USDC_POOL: Pubkey = pubkey!("Gf7sXMoP8iRw4iiXmJ1nq4vxcRycbGXy5RL8a8LnTd3v");

const PUMPFUN_PROGRAM_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");

const BUY_EXACT_BASE_OUT_IX: &str = "buy";
const BUY_EXACT_QUOTE_IN_IX: &str = "buy_exact_quote_in";
const SELL_IX: &str = "sell";
const MAX_BRIDGE_SLIPPAGE_BPS: u64 = 50;

#[derive(BorshDeserialize)]
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
enum BuyRoute {
    DirectSol,
    ViaUsdc,
}

pub struct PumpSwap {
    rpc: Arc<RpcClient>,
    fee_settings: OnceLock<FeeSettings>,
    sol_usdc_pool: Pubkey,
}

impl PumpSwap {
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
        let pool: PoolAccount = decode_account(&acc.data)
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
            quote_reserves: quote_balance.amount.parse()?,
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

    fn route_slippage(total_bps: u64) -> (u64, u64) {
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

    fn buy_route(pool: &PoolState) -> Result<BuyRoute> {
        match pool.quote_mint {
            WSOL => Ok(BuyRoute::DirectSol),
            USDC_MINT => Ok(BuyRoute::ViaUsdc),
            quote_mint => Err(anyhow!(
                "pumpswap buy does not support quote mint {quote_mint}"
            )),
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
        let bridge_quote =
            self.compute_quote(bridge_pool, fees, Side::Buy, p.amount, bridge_slippage);
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
        Ok(Quote {
            in_amount: p.amount,
            expected_out: target_quote.expected_out,
            min_out: target_quote.min_out,
            fee: bridge_quote.fee,
        })
    }

    async fn swap_via_usdc(
        &self,
        p: &Trade,
        target_pool_address: Pubkey,
        target_pool: &PoolState,
        fees: FeeSettings,
    ) -> Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
        let bridge_pool = self.load_sol_usdc_pool().await?;
        let (bridge_quote, target_quote) = self.route_quotes(p, &bridge_pool, target_pool, &fees);
        if bridge_quote.min_out == 0 || target_quote.min_out == 0 {
            return Err(anyhow!("pumpswap USDC route output is zero"));
        }

        let mut bridge_trade = *p;
        bridge_trade.mint = USDC_MINT;
        bridge_trade.pool = Some(self.sol_usdc_pool);
        let mut instructions = self.buy_instructions(
            &bridge_trade,
            self.sol_usdc_pool,
            &bridge_pool,
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
        Ok((instructions, vec![]))
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
        if p.side == Side::Buy && Self::buy_route(&pool)? == BuyRoute::ViaUsdc {
            return self.quote_via_usdc(p, &pool, &fees).await;
        }
        Ok(self.compute_quote(&pool, &fees, p.side, p.amount, p.slippage_bps))
    }

    async fn swap(&self, p: &Trade) -> Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
        let pool_address = p.pool.unwrap_or_else(|| Self::canonical_pool_pda(&p.mint));
        let (pool, fees) = (
            self.load_pool(&pool_address, &p.mint).await?,
            self.fee_settings().await?,
        );
        if p.side == Side::Buy && Self::buy_route(&pool)? == BuyRoute::ViaUsdc {
            return self.swap_via_usdc(p, pool_address, &pool, fees).await;
        }
        let quote = self.compute_quote(&pool, &fees, p.side, p.amount, p.slippage_bps);
        let user_quote_ata = ata(&p.wallet, &pool.quote_mint, &pool.quote_token_program);
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
            Side::Sell => {
                let mut data = Vec::with_capacity(24);
                data.extend_from_slice(&anchor_discriminator(SELL_IX));
                data.extend_from_slice(&p.amount.to_le_bytes());
                data.extend_from_slice(&quote.min_out.to_le_bytes());
                let swap_ix = Instruction {
                    program_id: PROGRAM_ID,
                    accounts: self.swap_accounts(p, pool_address, &pool, fees),
                    data,
                };

                let mut instructions = vec![
                    create_ata_idempotent(
                        &p.wallet,
                        &p.wallet,
                        &pool.quote_mint,
                        &pool.quote_token_program,
                    ),
                    swap_ix,
                ];
                if pool.quote_mint == WSOL {
                    instructions.push(close_account(&user_quote_ata, &p.wallet, &p.wallet));
                }
                instructions
            }
        };
        Ok((instructions, vec![]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Venue;

    fn test_fees() -> FeeSettings {
        FeeSettings {
            standard_protocol_recipient: Pubkey::new_unique(),
            mayhem_protocol_recipient: Pubkey::new_unique(),
            buyback_recipient: Pubkey::new_unique(),
            lp_bps: 20,
            protocol_bps: 5,
            creator_bps: 0,
        }
    }

    fn test_pool(base_mint: Pubkey, quote_mint: Pubkey) -> PoolState {
        PoolState {
            coin_creator: Pubkey::default(),
            base_mint,
            quote_mint,
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
            base_reserves: 1_000_000_000_000,
            quote_reserves: 10_000_000_000_000,
            is_mayhem: false,
            is_cashback: false,
            base_token_program: TOKEN_PROGRAM,
            quote_token_program: TOKEN_PROGRAM,
        }
    }

    #[test]
    fn selects_reserved_protocol_recipient_for_mayhem_pool() {
        let fees = FeeSettings {
            standard_protocol_recipient: PROGRAM_ID,
            mayhem_protocol_recipient: FEE_PROGRAM_ID,
            buyback_recipient: Pubkey::default(),
            lp_bps: 0,
            protocol_bps: 0,
            creator_bps: 0,
        };

        assert_eq!(fees.protocol_recipient(false), PROGRAM_ID);
        assert_eq!(fees.protocol_recipient(true), FEE_PROGRAM_ID);
    }

    #[test]
    fn route_slippage_reserves_at_most_fifty_bps_for_the_bridge() {
        assert_eq!(PumpSwap::route_slippage(30), (15, 15));
        assert_eq!(PumpSwap::route_slippage(500), (50, 450));
    }

    #[test]
    fn selects_buy_route_from_the_target_pool_quote_mint() {
        let mint = Pubkey::new_unique();

        assert_eq!(
            PumpSwap::buy_route(&test_pool(mint, WSOL)).unwrap(),
            BuyRoute::DirectSol
        );
        assert_eq!(
            PumpSwap::buy_route(&test_pool(mint, USDC_MINT)).unwrap(),
            BuyRoute::ViaUsdc
        );
        assert!(PumpSwap::buy_route(&test_pool(mint, Pubkey::new_unique())).is_err());
    }

    #[test]
    fn bridge_buys_exact_usdc_with_a_capped_sol_spend() {
        let wallet = Pubkey::new_unique();
        let trade = Trade::buy(wallet, USDC_MINT, 1_000_000, 300, Some(Venue::PumpSwap));
        let pool = test_pool(USDC_MINT, WSOL);
        let dex = PumpSwap::new(Arc::new(RpcClient::new(String::new())));
        let instructions = dex.buy_instructions(
            &trade,
            DEFAULT_SOL_USDC_POOL,
            &pool,
            test_fees(),
            BuyAmounts::ExactBaseOut {
                base_out: 99_000,
                max_quote_in: 1_000_000,
            },
        );

        assert_eq!(instructions.len(), 6);
        assert_eq!(instructions[2].program_id, SYSTEM_PROGRAM);
        assert_eq!(instructions[3].program_id, TOKEN_PROGRAM);
        assert_eq!(instructions[4].program_id, PROGRAM_ID);
        assert_eq!(instructions[5].program_id, TOKEN_PROGRAM);
        assert_eq!(
            &instructions[4].data[..8],
            &anchor_discriminator(BUY_EXACT_BASE_OUT_IX)
        );
        assert_eq!(
            u64::from_le_bytes(instructions[4].data[8..16].try_into().unwrap()),
            99_000
        );
        assert_eq!(
            u64::from_le_bytes(instructions[4].data[16..24].try_into().unwrap()),
            1_000_000
        );
    }

    #[test]
    fn target_leg_spends_the_exact_bridge_output() {
        let wallet = Pubkey::new_unique();
        let target_mint = Pubkey::new_unique();
        let route = Trade::buy(wallet, target_mint, 10_000_000, 500, Some(Venue::PumpSwap));
        let bridge_pool = test_pool(USDC_MINT, WSOL);
        let target_pool = test_pool(target_mint, USDC_MINT);
        let fees = test_fees();
        let dex = PumpSwap::new(Arc::new(RpcClient::new(String::new())));
        let (bridge_quote, target_quote) =
            dex.route_quotes(&route, &bridge_pool, &target_pool, &fees);

        let instructions = dex.buy_instructions(
            &route,
            Pubkey::new_unique(),
            &target_pool,
            fees,
            BuyAmounts::ExactQuoteIn {
                quote_in: bridge_quote.min_out,
                min_base_out: target_quote.min_out,
            },
        );
        let swap = instructions.last().unwrap();

        assert_eq!(
            u64::from_le_bytes(swap.data[8..16].try_into().unwrap()),
            bridge_quote.min_out
        );
    }

    #[tokio::test]
    #[ignore = "live mainnet RPC"]
    async fn quote_buy_from_usdc_pool() {
        let rpc = Arc::new(RpcClient::new(
            "https://api.mainnet-beta.solana.com".to_string(),
        ));
        let dex = PumpSwap::new(rpc);
        let wallet: Pubkey = "HPkBhdBS8tEfHbsWK1v2f82cPYrXHKZyr29apDbTttuD"
            .parse()
            .unwrap();
        let mint: Pubkey = "aHwwJn74ttpoHxzsrc1UhNHSjxyDAggh1sULqC3pump"
            .parse()
            .unwrap();
        let pool: Pubkey = "EwYm6KmxzpWuwthzAAMd3ND8Bp5TJX9hWnArJisV2TPQ"
            .parse()
            .unwrap();
        let trade = Trade::buy(wallet, mint, 1_000_000, 500, Some(Venue::PumpSwap)).with_pool(pool);

        let quote = dex.quote(&trade).await.unwrap();
        let (instructions, _) = dex.swap(&trade).await.unwrap();
        let swaps = instructions
            .iter()
            .filter(|instruction| instruction.program_id == PROGRAM_ID)
            .collect::<Vec<_>>();

        assert!(quote.expected_out > 0);
        assert!(quote.min_out > 0);
        assert_eq!(swaps.len(), 2);
        assert_eq!(
            &swaps[0].data[..8],
            &anchor_discriminator(BUY_EXACT_BASE_OUT_IX)
        );
        assert_eq!(
            &swaps[1].data[..8],
            &anchor_discriminator(BUY_EXACT_QUOTE_IN_IX)
        );
        assert_eq!(&swaps[0].data[8..16], &swaps[1].data[8..16]);

        let blockhash = dex.rpc.get_latest_blockhash().await.unwrap();
        let message =
            solana_message::v0::Message::try_compile(&wallet, &instructions, &[], blockhash)
                .unwrap();
        let transaction = solana_transaction::versioned::VersionedTransaction {
            signatures: vec![solana_signature::Signature::default()],
            message: solana_message::VersionedMessage::V0(message),
        };
        assert!(bincode::serialized_size(&transaction).unwrap() > 1_232);

        let mut lookup_addresses = Vec::new();
        for account in instructions
            .iter()
            .flat_map(|instruction| &instruction.accounts)
            .filter(|account| !account.is_signer)
        {
            if !lookup_addresses.contains(&account.pubkey) {
                lookup_addresses.push(account.pubkey);
            }
        }
        let lookup_table = AddressLookupTableAccount {
            key: Pubkey::new_unique(),
            addresses: lookup_addresses,
        };
        let compressed_message = solana_message::v0::Message::try_compile(
            &wallet,
            &instructions,
            &[lookup_table],
            blockhash,
        )
        .unwrap();
        let compressed_transaction = solana_transaction::versioned::VersionedTransaction {
            signatures: vec![solana_signature::Signature::default()],
            message: solana_message::VersionedMessage::V0(compressed_message),
        };
        assert!(bincode::serialized_size(&compressed_transaction).unwrap() <= 1_232);
    }

    #[tokio::test]
    #[ignore = "live mainnet RPC"]
    async fn quote_buy_1_sol() {
        let rpc = Arc::new(RpcClient::new(
            "https://api.mainnet-beta.solana.com".to_string(),
        ));
        let dex = PumpSwap::new(rpc);
        let mint: Pubkey = "9JihXt4NZtZzURoMm1KrGN6y2a9LH9xdKkh5p9kJpump"
            .parse()
            .unwrap();

        let params = Trade::buy(
            Pubkey::default(),
            mint,
            1_000_000_000,
            300,
            Some(Venue::PumpSwap),
        );

        match dex.quote(&params).await {
            Ok(q) => {
                println!(
                    "pumpswap buy 1 SOL → expected {} tokens (min {}), fee {} lamports",
                    q.expected_out, q.min_out, q.fee
                );
                assert!(q.expected_out > 0, "expected nonzero token output");
            }
            Err(e) => println!(
                "pumpswap: no quote (not a canonical WSOL pool, or still on the curve): {e}"
            ),
        }
    }

    #[tokio::test]
    #[ignore = "live mainnet RPC"]
    async fn simulate_mayhem_buy() {
        use solana_client::rpc_config::RpcSimulateTransactionConfig;
        use solana_message::{VersionedMessage, v0};
        use solana_signature::Signature;
        use solana_transaction::versioned::VersionedTransaction;

        let rpc = Arc::new(RpcClient::new(
            "https://api.mainnet-beta.solana.com".to_string(),
        ));
        let dex = PumpSwap::new(rpc.clone());
        let mint: Pubkey = "HXTaBKp2qa5n2DAzzc949tAJMmuVyRoCQCDNkFKkpump"
            .parse()
            .unwrap();
        let pool: Pubkey = "7aN5B42L5bLTvxoGLCScdqU46o4C1j4zKxmjjrmwQKuR"
            .parse()
            .unwrap();
        let wallet: Pubkey = "7a1xV8pUaJbUMqGVC3Z2NQbhW5pBJT2UXfiSFtuUC18S"
            .parse()
            .unwrap();

        assert!(dex.load_pool(&pool, &mint).await.unwrap().is_mayhem);

        let params = Trade::buy(wallet, mint, 100_000, 500, Some(Venue::PumpSwap)).with_pool(pool);
        let mut instructions = vec![set_compute_unit_limit(350_000), set_compute_unit_price(0)];
        instructions.extend(dex.swap(&params).await.unwrap().0);

        let blockhash = rpc.get_latest_blockhash().await.unwrap();
        let msg = v0::Message::try_compile(&wallet, &instructions, &[], blockhash).unwrap();
        let tx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V0(msg),
        };
        let simulation = rpc
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

        for log in simulation.logs.unwrap_or_default() {
            println!("  {log}");
        }
        assert_eq!(simulation.err, None);
    }
}
