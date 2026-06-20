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

const PUMPFUN_PROGRAM_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");

const BUY_IX: &str = "buy_exact_quote_in";
const SELL_IX: &str = "sell";

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
    protocol_recipient: Pubkey,
    buyback_recipient: Pubkey,
    lp_bps: u64,
    protocol_bps: u64,
    creator_bps: u64,
}

struct PoolState {
    coin_creator: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    base_reserves: u64,
    quote_reserves: u64,
    is_cashback: bool,

    base_token_program: Pubkey,
    quote_token_program: Pubkey,
}

pub struct PumpSwap {
    rpc: Arc<RpcClient>,
    fee_settings: OnceLock<FeeSettings>,
}

impl PumpSwap {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self {
            rpc,
            fee_settings: OnceLock::new(),
        }
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
            protocol_recipient: gc.protocol_fee_recipients[0],
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
        protocol_recipient: Pubkey,
    ) -> Vec<AccountMeta> {
        let creator_vault_authority = Self::coin_creator_vault_authority_pda(&pool.coin_creator);
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
        Ok(self.compute_quote(&pool, &fees, p.side, p.amount, p.slippage_bps))
    }

    async fn swap(&self, p: &Trade) -> Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
        let pool_address = p.pool.unwrap_or_else(|| Self::canonical_pool_pda(&p.mint));
        let (pool, fees) = (
            self.load_pool(&pool_address, &p.mint).await?,
            self.fee_settings().await?,
        );
        let quote = self.compute_quote(&pool, &fees, p.side, p.amount, p.slippage_bps);
        let wrap_sol = pool.quote_mint == WSOL;
        let user_quote_ata = ata(&p.wallet, &pool.quote_mint, &pool.quote_token_program);

        let mut accounts = self.named_accounts(p, pool_address, &pool, fees.protocol_recipient);
        if p.side == Side::Buy {
            accounts.push(AccountMeta::new_readonly(Self::global_volume_pda(), false));
            accounts.push(AccountMeta::new(Self::user_volume_pda(&p.wallet), false));
        }
        accounts.push(AccountMeta::new_readonly(Self::fee_config_pda(), false));
        accounts.push(AccountMeta::new_readonly(FEE_PROGRAM_ID, false));
        accounts.extend(self.remaining_accounts(p, &pool, &fees, p.side));

        let mut data = Vec::with_capacity(25);
        let instructions = match p.side {
            Side::Buy => {
                data.extend_from_slice(&anchor_discriminator(BUY_IX));
                data.extend_from_slice(&p.amount.to_le_bytes());
                data.extend_from_slice(&quote.min_out.to_le_bytes());
                data.push(1);
                let swap_ix = Instruction {
                    program_id: PROGRAM_ID,
                    accounts,
                    data,
                };

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
                if wrap_sol {
                    instructions.push(system_transfer(&p.wallet, &user_quote_ata, p.amount));
                    instructions.push(sync_native(&user_quote_ata));
                }
                instructions.push(swap_ix);
                if wrap_sol {
                    instructions.push(close_account(&user_quote_ata, &p.wallet, &p.wallet));
                }
                instructions
            }
            Side::Sell => {
                data.extend_from_slice(&anchor_discriminator(SELL_IX));
                data.extend_from_slice(&p.amount.to_le_bytes());
                data.extend_from_slice(&quote.min_out.to_le_bytes());
                let swap_ix = Instruction {
                    program_id: PROGRAM_ID,
                    accounts,
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
                if wrap_sol {
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
}
