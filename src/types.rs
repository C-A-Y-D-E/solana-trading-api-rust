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

#[derive(Debug, Clone, Copy)]
pub struct Trade {
    pub wallet: Pubkey,

    pub mint: Pubkey,
    pub side: Side,

    pub amount: u64,

    pub slippage_bps: u64,

    pub venue: Option<Venue>,

    pub pool: Option<Pubkey>,
}

impl Trade {
    pub fn buy(
        wallet: Pubkey,
        mint: Pubkey,
        amount: u64,
        slippage_bps: u64,
        venue: Option<Venue>,
    ) -> Self {
        Self {
            wallet,
            mint,
            side: Side::Buy,
            amount,
            slippage_bps,
            venue,
            pool: None,
        }
    }

    pub fn sell(
        wallet: Pubkey,
        mint: Pubkey,
        amount: u64,
        slippage_bps: u64,
        venue: Option<Venue>,
    ) -> Self {
        Self {
            wallet,
            mint,
            side: Side::Sell,
            amount,
            slippage_bps,
            venue,
            pool: None,
        }
    }

    pub fn with_pool(mut self, pool: Pubkey) -> Self {
        self.pool = Some(pool);
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub in_amount: u64,

    pub expected_out: u64,

    pub min_out: u64,

    pub fee: u64,
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
}

#[async_trait]
pub trait Dex: Send + Sync {
    fn name(&self) -> &'static str;
    async fn quote(&self, trade: &Trade) -> anyhow::Result<Quote>;

    async fn swap(
        &self,
        trade: &Trade,
    ) -> anyhow::Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)>;
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
