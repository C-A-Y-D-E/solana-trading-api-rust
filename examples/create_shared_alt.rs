use std::{env, fs, io::ErrorKind, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail, ensure};
use solana_address_lookup_table_interface::{
    instruction::{create_lookup_table, extend_lookup_table},
    program,
    state::{AddressLookupTable, LOOKUP_TABLE_MAX_ADDRESSES, LOOKUP_TABLE_META_SIZE},
};
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signer::Signer as _;
use solana_trading_api::{DEFAULT_SOL_USDC_POOL, Pubkey, RpcClient, shared_lookup_addresses};
use solana_transaction::Transaction;

const PRIVATE_KEY_ENV: &str = "PRIVATE_KEY";
const RPC_URL_ENV: &str = "RPC_URL";
const BRIDGE_POOL_ENV: &str = "PUMPSWAP_SOL_USDC_POOL";
const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const EXTEND_CHUNK_SIZE: usize = 20;
const ACTIVATION_ATTEMPTS: usize = 30;
const SLOT_HASHES_SYSVAR: Pubkey =
    solana_pubkey::pubkey!("SysvarS1otHashes111111111111111111111111111");

struct Options {
    dry_run: bool,
    existing_table: Option<Pubkey>,
}

impl Options {
    fn parse() -> Result<Self> {
        let mut options = Self {
            dry_run: false,
            existing_table: None,
        };
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--dry-run" => options.dry_run = true,
                "--table" => {
                    options.existing_table = Some(
                        args.next()
                            .context("--table needs an address")?
                            .parse()
                            .context("invalid table address")?,
                    );
                }
                _ => bail!(
                    "usage: cargo run --example create_shared_alt -- [--dry-run] [--table ADDRESS]"
                ),
            }
        }
        Ok(options)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse()?;
    let rpc_url = setting(RPC_URL_ENV)?.unwrap_or_else(|| DEFAULT_RPC_URL.into());
    let bridge = setting(BRIDGE_POOL_ENV)?
        .map(|value| value.parse().context("invalid bridge pool"))
        .transpose()?
        .unwrap_or(DEFAULT_SOL_USDC_POOL);
    let rpc = Arc::new(RpcClient::new(rpc_url));
    let addresses = shared_lookup_addresses(rpc.clone(), bridge).await?;
    ensure!(
        addresses.len() <= LOOKUP_TABLE_MAX_ADDRESSES,
        "shared addresses exceed table capacity"
    );
    println!("Shared Pump.fun/PumpSwap addresses: {}", addresses.len());
    println!("Fixed SOL/USDC bridge: {bridge}");
    if options.dry_run {
        for address in &addresses {
            println!("{address}");
        }
        println!("Preview only; no key loaded or transactions sent.");
        return Ok(());
    }

    let authority = load_keypair()?;
    println!("ALT authority/payer: {}", authority.pubkey());
    let creator = LookupTableCreator {
        rpc: &rpc,
        authority: &authority,
    };
    let table = creator.populate(options.existing_table, &addresses).await?;
    println!("ALT_ADDRESS={table}");
    println!("Ready for any trading wallet; no target token or pool entries required.");
    Ok(())
}

struct LookupTableCreator<'a> {
    rpc: &'a RpcClient,
    authority: &'a Keypair,
}

impl LookupTableCreator<'_> {
    async fn populate(&self, existing: Option<Pubkey>, desired: &[Pubkey]) -> Result<Pubkey> {
        let stored = match existing {
            Some(table) => self.read_owned_table(table).await?,
            None => vec![],
        };
        let missing: Vec<_> = desired
            .iter()
            .copied()
            .filter(|key| !stored.contains(key))
            .collect();
        ensure!(
            stored.len() + missing.len() <= LOOKUP_TABLE_MAX_ADDRESSES,
            "table is full"
        );
        let rent = self
            .rpc
            .get_minimum_balance_for_rent_exemption(
                LOOKUP_TABLE_META_SIZE + (stored.len() + missing.len()) * 32,
            )
            .await?;
        let current_rent = match existing {
            Some(table) => self.rpc.get_balance(&table).await?,
            None => 0,
        };
        let transaction_count =
            missing.len().div_ceil(EXTEND_CHUNK_SIZE) + usize::from(existing.is_none());
        self.check_funding(rent.saturating_sub(current_rent), transaction_count)
            .await?;
        let table = match existing {
            Some(table) => table,
            None => self.create().await?,
        };
        println!(
            "Adding {} addresses. Resume with --table {table} if interrupted.",
            missing.len()
        );
        for chunk in missing.chunks(EXTEND_CHUNK_SIZE) {
            self.send(extend_lookup_table(
                table,
                self.authority.pubkey(),
                Some(self.authority.pubkey()),
                chunk.to_vec(),
            ))
            .await
            .with_context(|| format!("extend {table}; resume using --table {table}"))?;
        }
        self.wait_until_active(table, desired).await?;
        Ok(table)
    }

    async fn create(&self) -> Result<Pubkey> {
        let slot = recent_creation_slot(self.rpc).await?;
        let authority = self.authority.pubkey();
        let (instruction, table) = create_lookup_table(authority, authority, slot);
        println!("Table address: {table}");
        self.send(instruction).await.context("create table")?;
        Ok(table)
    }

    async fn check_funding(&self, additional_rent: u64, transaction_count: usize) -> Result<()> {
        let blockhash = self.rpc.get_latest_blockhash().await?;
        let message = solana_message::Message::new_with_blockhash(
            &[],
            Some(&self.authority.pubkey()),
            &blockhash,
        );
        let fees = self.rpc.get_fee_for_message(&message).await? * transaction_count as u64;
        let needed = additional_rent + fees;
        let balance = self.rpc.get_balance(&self.authority.pubkey()).await?;
        println!(
            "Additional rent: {additional_rent}; estimated fees: {fees}; payer balance: {balance} lamports"
        );
        // Balance and fees can change after this check; preflight remains enabled on every send.
        ensure!(
            balance >= needed,
            "insufficient SOL: have {balance} lamports, need about {needed}; short {} lamports. Fund the payer and resume the existing table with --table ADDRESS",
            needed.saturating_sub(balance)
        );
        Ok(())
    }

    async fn read_owned_table(&self, table: Pubkey) -> Result<Vec<Pubkey>> {
        let account = self.rpc.get_account(&table).await?;
        ensure!(program::check_id(&account.owner), "account is not an ALT");
        let state = AddressLookupTable::deserialize(&account.data)?;
        ensure!(
            state.meta.authority == Some(self.authority.pubkey()),
            "wrong authority or frozen table"
        );
        ensure!(
            state.meta.deactivation_slot == u64::MAX,
            "table is deactivating"
        );
        Ok(state.addresses.into_owned())
    }

    async fn send(&self, instruction: Instruction) -> Result<()> {
        let blockhash = self.rpc.get_latest_blockhash().await?;
        let mut transaction =
            Transaction::new_with_payer(&[instruction], Some(&self.authority.pubkey()));
        transaction.try_sign(&[self.authority], blockhash)?;
        // Print before sending so an ambiguous RPC error can be checked without recreating a table.
        println!("Transaction: {}", transaction.signatures[0]);
        self.rpc
            .send_and_confirm_transaction(&transaction)
            .await
            .context("send and confirm ALT transaction")?;
        Ok(())
    }

    async fn wait_until_active(&self, table: Pubkey, desired: &[Pubkey]) -> Result<()> {
        for _ in 0..ACTIVATION_ATTEMPTS {
            let account = self.rpc.get_account(&table).await?;
            let state = AddressLookupTable::deserialize(&account.data)?;
            if desired.iter().all(|key| state.addresses.contains(key))
                && self.rpc.get_slot().await? > state.meta.last_extended_slot
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        bail!("table {table} not yet active; retry with --table {table}")
    }
}

async fn recent_creation_slot(rpc: &RpcClient) -> Result<u64> {
    // ALT creation validates membership in SlotHashes, not merely distance from getSlot().
    let account = rpc
        .get_account(&SLOT_HASHES_SYSVAR)
        .await
        .context("read SlotHashes sysvar")?;
    let hashes: solana_slot_hashes::SlotHashes =
        bincode::deserialize(&account.data).context("decode SlotHashes sysvar")?;
    hashes
        .first()
        .map(|(slot, _)| *slot)
        .context("SlotHashes is empty")
}

#[cfg(test)]
#[path = "../tests/unit/examples/create_shared_alt.rs"]
mod tests;

fn load_keypair() -> Result<Keypair> {
    let key = setting(PRIVATE_KEY_ENV)?
        .ok_or_else(|| anyhow!("set {PRIVATE_KEY_ENV} in the environment or .env"))?;
    Keypair::try_from_base58_string(&key)
        .map_err(|_| anyhow!("{PRIVATE_KEY_ENV} must be a valid base58 keypair"))
}

fn setting(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => return Ok(Some(value)),
        Err(env::VarError::NotUnicode(_)) => bail!("{name} is not valid Unicode"),
        Err(env::VarError::NotPresent) => {}
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    Ok(contents.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        (key.trim() == name).then(|| value.trim().trim_matches(['\'', '"']).to_string())
    }))
}
