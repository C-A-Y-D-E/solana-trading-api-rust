use borsh::BorshDeserialize;
use sha2::{Digest, Sha256};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{Pubkey, pubkey};

pub const SYSTEM_PROGRAM: Pubkey = pubkey!("11111111111111111111111111111111");
pub const TOKEN_PROGRAM: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ATA_PROGRAM: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const COMPUTE_BUDGET_PROGRAM: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");
pub const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

const IX_CLOSE_ACCOUNT: u8 = 9;
const IX_SYNC_NATIVE: u8 = 17;

const SYS_TRANSFER: u32 = 2;

const ATA_CREATE_IDEMPOTENT: u8 = 1;

pub fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

pub fn ata(wallet: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    pda(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
}

pub fn create_ata_idempotent(
    payer: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata(wallet, mint, token_program), false),
            AccountMeta::new_readonly(*wallet, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![ATA_CREATE_IDEMPOTENT],
    }
}

pub fn system_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&SYS_TRANSFER.to_le_bytes());
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

pub fn tip(payer: &Pubkey, tip_account: &Pubkey, lamports: u64) -> Instruction {
    system_transfer(payer, tip_account, lamports)
}

pub fn sync_native(account: &Pubkey) -> Instruction {
    Instruction {
        program_id: TOKEN_PROGRAM,
        accounts: vec![AccountMeta::new(*account, false)],
        data: vec![IX_SYNC_NATIVE],
    }
}

pub fn close_account(account: &Pubkey, destination: &Pubkey, owner: &Pubkey) -> Instruction {
    Instruction {
        program_id: TOKEN_PROGRAM,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![IX_CLOSE_ACCOUNT],
    }
}

pub fn set_compute_unit_limit(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(2);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM,
        accounts: vec![],
        data,
    }
}

pub fn set_compute_unit_price(micro_lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3);
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM,
        accounts: vec![],
        data,
    }
}

pub fn decode_account<T: BorshDeserialize>(data: &[u8]) -> anyhow::Result<T> {
    let mut reader: &[u8] = data;
    Ok(T::deserialize_reader(&mut reader)?)
}

pub fn anchor_discriminator(ix_name: &str) -> [u8; 8] {
    let digest = Sha256::new()
        .chain_update(b"global:")
        .chain_update(ix_name.as_bytes())
        .finalize();
    let mut disc = [0u8; 8];
    disc.copy_from_slice(&digest[..8]);
    disc
}

pub fn base_out_for_quote_in(quote_reserve: u64, base_reserve: u64, quote_in: u64) -> u64 {
    if quote_in == 0 || quote_reserve == 0 || base_reserve == 0 {
        return 0;
    }
    ((base_reserve as u128 * quote_in as u128) / (quote_reserve as u128 + quote_in as u128)) as u64
}

pub fn quote_out_for_base_in(quote_reserve: u64, base_reserve: u64, base_in: u64) -> u64 {
    if base_in == 0 || quote_reserve == 0 || base_reserve == 0 {
        return 0;
    }
    ((quote_reserve as u128 * base_in as u128) / (base_reserve as u128 + base_in as u128)) as u64
}

pub fn fee_floor(amount: u64, bps: u64) -> u64 {
    (amount as u128 * bps as u128 / 10_000) as u64
}

pub fn fee_ceil(amount: u64, bps: u64) -> u64 {
    (amount as u128 * bps as u128).div_ceil(10_000) as u64
}

pub fn slippage_up(x: u64, bps: u64) -> u64 {
    x + (x as u128 * bps as u128 / 10_000) as u64
}

pub fn slippage_down(x: u64, bps: u64) -> u64 {
    x - (x as u128 * bps as u128 / 10_000) as u64
}

#[cfg(test)]
mod tests {
    use super::anchor_discriminator;

    #[test]
    fn discriminators_match_idl() {
        assert_eq!(
            anchor_discriminator("buy_exact_sol_in"),
            [56, 252, 116, 8, 158, 223, 205, 95]
        );
        assert_eq!(
            anchor_discriminator("buy_exact_quote_in"),
            [198, 46, 21, 82, 180, 217, 232, 112]
        );
        assert_eq!(
            anchor_discriminator("sell"),
            [51, 230, 133, 164, 1, 127, 131, 173]
        );
    }
}
