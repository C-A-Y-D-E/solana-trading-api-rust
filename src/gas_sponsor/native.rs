use std::collections::HashMap;

use anyhow::{Result, ensure};
use solana_instruction::{AccountMeta, Instruction};
use spl_token::solana_program::program_pack::Pack;

use crate::dexes::common::*;
use crate::dexes::{pumpfun, pumpswap};
use crate::{PreparedSwap, Pubkey, RpcClient};

pub(crate) const INIT_VOLUME: &str = "init_user_volume_accumulator";

pub(crate) async fn fund_native_setup(
    rpc: &RpcClient,
    mut prepared: PreparedSwap,
    user: Pubkey,
    payer: Pubkey,
) -> Result<PreparedSwap> {
    let mut setup = NativeSetup {
        rpc,
        user,
        payer,
        accounts: HashMap::new(),
        instructions: Vec::new(),
        wsol_refund: 0,
    };
    for instruction in prepared.instructions {
        setup.append(instruction).await?;
    }
    prepared.instructions = setup.instructions;
    Ok(prepared)
}

#[derive(Clone, Copy)]
struct SetupAccount {
    initialized: bool,
    lamports: u64,
}

struct NativeSetup<'a> {
    rpc: &'a RpcClient,
    user: Pubkey,
    payer: Pubkey,
    accounts: HashMap<Pubkey, SetupAccount>,
    instructions: Vec<Instruction>,
    wsol_refund: u64,
}

impl NativeSetup<'_> {
    async fn append(&mut self, instruction: Instruction) -> Result<()> {
        match instruction.program_id {
            ATA_PROGRAM => {
                ensure!(
                    instruction.data == [1] && instruction.accounts.len() == 6,
                    "unsupported native ATA instruction"
                );
                self.create_ata(
                    instruction.accounts[2].pubkey,
                    instruction.accounts[3].pubkey,
                    instruction.accounts[5].pubkey,
                )
                .await?;
                return Ok(());
            }
            pumpswap::PROGRAM_ID => self.pumpswap_setup(&instruction).await?,
            pumpfun::PROGRAM_ID => self.pumpfun_setup(&instruction).await?,
            _ => {}
        }
        let closes_wsol = instruction.program_id == TOKEN_PROGRAM
            && instruction.data == [9]
            && instruction
                .accounts
                .first()
                .is_some_and(|account| account.pubkey == ata(&self.user, &WSOL, &TOKEN_PROGRAM));
        if closes_wsol {
            ensure!(
                instruction.accounts.len() == 3
                    && instruction.accounts[1].pubkey == self.user
                    && instruction.accounts[2].pubkey == self.user,
                "unexpected WSOL close destination or authority"
            );
        }
        self.instructions.push(instruction);
        if closes_wsol {
            // Return only this route's sponsor-funded deposit, never the user's existing WSOL rent or proceeds.
            if self.wsol_refund > 0 {
                self.instructions
                    .push(system_transfer(&self.user, &self.payer, self.wsol_refund));
                self.wsol_refund = 0;
            }
            self.accounts.insert(
                ata(&self.user, &WSOL, &TOKEN_PROGRAM),
                SetupAccount {
                    initialized: false,
                    lamports: 0,
                },
            );
        }
        Ok(())
    }

    async fn account(&mut self, address: Pubkey, owner: Pubkey) -> Result<SetupAccount> {
        if let Some(account) = self.accounts.get(&address) {
            return Ok(*account);
        }
        let response = self
            .rpc
            .get_account_with_commitment(&address, self.rpc.commitment())
            .await?;
        let account = match response.value {
            Some(account) => {
                ensure!(
                    account.owner == owner
                        || (account.owner == SYSTEM_PROGRAM && account.data.is_empty()),
                    "unexpected owner for native setup account {address}"
                );
                SetupAccount {
                    initialized: account.owner == owner && !account.data.is_empty(),
                    lamports: account.lamports,
                }
            }
            None => SetupAccount {
                initialized: false,
                lamports: 0,
            },
        };
        self.accounts.insert(address, account);
        Ok(account)
    }

    async fn create_ata(
        &mut self,
        owner: Pubkey,
        mint: Pubkey,
        token_program: Pubkey,
    ) -> Result<()> {
        let address = ata(&owner, &mint, &token_program);
        let account = self.account(address, token_program).await?;
        if account.initialized {
            return Ok(());
        }
        let mut instruction = create_ata_idempotent(&self.payer, &owner, &mint, &token_program);
        if owner == self.user && mint == WSOL && token_program == TOKEN_PROGRAM {
            let rent = self
                .rpc
                .get_minimum_balance_for_rent_exemption(spl_token::state::Account::LEN)
                .await?;
            self.wsol_refund = rent.saturating_sub(account.lamports);
            // Fail if someone initialized WSOL meanwhile; do not refund somebody else's deposit.
            instruction.data = vec![0];
        }
        self.instructions.push(instruction);
        self.accounts.insert(
            address,
            SetupAccount {
                initialized: true,
                ..account
            },
        );
        Ok(())
    }

    async fn init_volume(&mut self, instruction: &Instruction) -> Result<()> {
        let program = instruction.program_id;
        let volume = pda(&[b"user_volume_accumulator", self.user.as_ref()], &program);
        if !instruction
            .accounts
            .iter()
            .any(|account| account.pubkey == volume)
            || self.account(volume, program).await?.initialized
        {
            return Ok(());
        }
        // Both bundled IDLs expose a separate payer on init_user_volume_accumulator.
        self.instructions.push(Instruction {
            program_id: program,
            accounts: vec![
                AccountMeta::new(self.payer, true),
                AccountMeta::new_readonly(self.user, false),
                AccountMeta::new(volume, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
                AccountMeta::new_readonly(pda(&[b"__event_authority"], &program), false),
                AccountMeta::new_readonly(program, false),
            ],
            data: anchor_discriminator(INIT_VOLUME).to_vec(),
        });
        self.accounts.insert(
            volume,
            SetupAccount {
                initialized: true,
                lamports: 0,
            },
        );
        Ok(())
    }

    async fn pumpswap_setup(&mut self, instruction: &Instruction) -> Result<()> {
        let accounts = &instruction.accounts;
        ensure!(
            accounts.len() >= 23
                && accounts[1].pubkey == self.user
                && [
                    pumpswap::BUY_EXACT_BASE_OUT_IX,
                    pumpswap::BUY_EXACT_QUOTE_IN_IX,
                    pumpswap::SELL_IX
                ]
                .iter()
                .any(|name| instruction.data.starts_with(&anchor_discriminator(name))),
            "unsupported native PumpSwap instruction"
        );
        let mint = accounts[4].pubkey;
        let token_program = accounts[12].pubkey;
        self.init_volume(instruction).await?;
        for owner in [
            accounts[9].pubkey,
            accounts[18].pubkey,
            accounts[accounts.len() - 2].pubkey,
        ] {
            self.create_ata(owner, mint, token_program).await?;
        }
        let volume = pda(
            &[b"user_volume_accumulator", self.user.as_ref()],
            &pumpswap::PROGRAM_ID,
        );
        let cashback_ata = ata(&volume, &mint, &token_program);
        if accounts
            .iter()
            .any(|account| account.pubkey == cashback_ata)
        {
            self.create_ata(volume, mint, token_program).await?;
        }
        Ok(())
    }

    async fn pumpfun_setup(&mut self, instruction: &Instruction) -> Result<()> {
        let accounts = &instruction.accounts;
        ensure!(
            accounts.len() >= 14
                && accounts[6].pubkey == self.user
                && [pumpfun::BUY_IX, pumpfun::SELL_IX]
                    .iter()
                    .any(|name| instruction.data.starts_with(&anchor_discriminator(name))),
            "unsupported native Pump.fun instruction"
        );
        self.init_volume(instruction).await?;
        let is_sell = instruction
            .data
            .starts_with(&anchor_discriminator(pumpfun::SELL_IX));
        let vault = accounts[if is_sell { 8 } else { 9 }].pubkey;
        let balance = self.account(vault, SYSTEM_PROGRAM).await?.lamports;
        let rent = self.rpc.get_minimum_balance_for_rent_exemption(0).await?;
        if balance < rent {
            self.instructions
                .push(system_transfer(&self.payer, &vault, rent - balance));
            self.accounts.insert(
                vault,
                SetupAccount {
                    initialized: false,
                    lamports: rent,
                },
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/gas_sponsor/native.rs"]
pub(crate) mod tests;
