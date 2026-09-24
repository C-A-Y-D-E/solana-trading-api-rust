use async_trait::async_trait;
use solana_instruction::Instruction;
use solana_message::AddressLookupTableAccount;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Venue {
    PumpFun,
    PumpSwap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    Sol,
    Usdc,
}

#[derive(Debug, Clone, Copy)]
pub struct Trade {
    pub settlement: Settlement,
    pub wallet: Pubkey,

    pub mint: Pubkey,
    pub side: Side,

    pub amount: u64,

    pub slippage_bps: u64,

    /// None compares Jupiter and configured DFlow; Some selects a native venue with fallback.
    pub venue: Option<Venue>,

    pub pool: Option<Pubkey>,
}

impl Trade {
    /// Buys with SOL lamports, bridging USDC-quoted PumpSwap pools automatically.
    pub fn buy(
        wallet: Pubkey,
        mint: Pubkey,
        amount: u64,
        slippage_bps: u64,
        venue: Option<Venue>,
    ) -> Self {
        Self {
            settlement: Settlement::Sol,
            wallet,
            mint,
            side: Side::Buy,
            amount,
            slippage_bps,
            venue,
            pool: None,
        }
    }

    /// Sells base-token units for SOL, bridging USDC-quoted PumpSwap pools automatically.
    pub fn sell(
        wallet: Pubkey,
        mint: Pubkey,
        amount: u64,
        slippage_bps: u64,
        venue: Option<Venue>,
    ) -> Self {
        Self {
            settlement: Settlement::Sol,
            wallet,
            mint,
            side: Side::Sell,
            amount,
            slippage_bps,
            venue,
            pool: None,
        }
    }

    /// Selects the native pool; USDC comparison or native failure may choose an aggregator instead.
    pub fn with_pool(mut self, pool: Pubkey) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Selects buy funding or sell proceeds: SOL lamports or USDC base units (6 decimals).
    pub fn with_settlement(mut self, settlement: Settlement) -> Self {
        self.settlement = settlement;
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub in_amount: u64,

    /// Estimated curve-only execution loss against pre-trade spot; 100 bps = 1%.
    /// Excludes fees, slippage buffers, rounding and unused intermediate balances.
    /// None means unavailable (including aggregators and capped bonding-curve buys), not zero.
    pub price_impact_bps: Option<f64>,

    pub expected_out: u64,

    pub min_out: u64,

    /// Venue fee in its quote currency; bridged routes report only the bridge fee in SOL lamports.
    /// Aggregators return zero here when fees are not separately itemized, not when trading is free.
    pub fee: u64,
    /// SDK fee in settlement base units: buy gross input or sell quoted expected output times the rate.
    /// Zero when disabled; outputs are already net of it. Not a percentage of actual sell proceeds.
    pub application_fee: u64,
    /// Reserved USDC sponsorship ceiling, already reflected in amounts; finalized before signing.
    pub sponsorship_fee: u64,
}

/// A quote and its instructions built from the same venue snapshot.
#[derive(Debug)]
pub struct PreparedSwap {
    pub venue: &'static str,
    pub quote: Quote,
    pub instructions: Vec<Instruction>,
    pub lookup_tables: Vec<AddressLookupTableAccount>,
}

#[derive(Debug, Clone, Copy)]
pub struct Tip {
    pub account: Pubkey,
    pub lamports: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapStatus {
    Confirmed,
    Failed,

    Pending,
}

#[derive(Debug, Clone)]
pub struct SwapResult {
    pub hash: String,

    pub dex: &'static str,
    pub status: SwapStatus,

    pub amount_received: Option<u64>,

    /// USDC charge encoded in the submitted transaction; collected only on successful execution.
    pub sponsorship_fee: u64,
}

#[async_trait]
pub trait Dex: Send + Sync {
    fn name(&self) -> &'static str;
    async fn quote(&self, trade: &Trade) -> anyhow::Result<Quote>;

    async fn swap(
        &self,
        trade: &Trade,
    ) -> anyhow::Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
        let prepared = self.prepare_swap(trade).await?;
        Ok((prepared.instructions, prepared.lookup_tables))
    }

    /// Must return the quote enforced by these instructions, without independently requoting.
    async fn prepare_swap(&self, trade: &Trade) -> anyhow::Result<PreparedSwap>;

    /// Requires explicit support for sponsored rent as well as network fees.
    async fn prepare_sponsored_swap(
        &self,
        _trade: &Trade,
        _payer: &Pubkey,
    ) -> anyhow::Result<PreparedSwap> {
        anyhow::bail!("{} does not support sponsored swaps", self.name())
    }
}

#[async_trait]
pub trait Signer: Send + Sync {
    async fn sign(&self, wallet: &Pubkey, tx: &VersionedTransaction) -> anyhow::Result<Signature>;
}

#[async_trait]
pub trait Submitter: Send + Sync {
    async fn submit(&self, tx: &VersionedTransaction) -> anyhow::Result<Signature>;

    fn default_tip(&self) -> Option<Tip> {
        None
    }
}
