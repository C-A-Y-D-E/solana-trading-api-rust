use std::error::Error as _;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_pubkey::{Pubkey, pubkey};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

use crate::types::{Submitter, Tip};

pub struct RpcSubmitter {
    rpc: Arc<RpcClient>,
}

impl RpcSubmitter {
    pub fn new(rpc: Arc<RpcClient>) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl Submitter for RpcSubmitter {
    async fn submit(&self, tx: &VersionedTransaction) -> Result<Signature> {
        Ok(self.rpc.send_transaction(tx).await?)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SubmitProtection {
    Low,
    Medium,
    High,
}

impl SubmitProtection {
    fn as_str(self) -> &'static str {
        match self {
            SubmitProtection::Low => "SP_LOW",
            SubmitProtection::Medium => "SP_MEDIUM",
            SubmitProtection::High => "SP_HIGH",
        }
    }
}

pub struct BloxrouteSubmitter {
    http: reqwest::Client,
    base_url: String,
    auth: String,
    front_running_protection: bool,
    submit_protection: SubmitProtection,
    use_staked_rpcs: bool,
    skip_preflight: bool,

    tip_account: Pubkey,
    tip_lamports: u64,
}

impl BloxrouteSubmitter {
    pub const DEFAULT_TIP_ACCOUNT: Pubkey = pubkey!("HWEoBxYs7ssKuudEjzjmpfJVX7Dvi7wescFsVx2L5yoY");

    pub const MIN_TIP_LAMPORTS: u64 = 1_000_000;

    pub fn mev(base_url: impl Into<String>, auth: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            auth: auth.into(),
            front_running_protection: true,
            submit_protection: SubmitProtection::Medium,
            use_staked_rpcs: false,
            skip_preflight: true,
            tip_account: Self::DEFAULT_TIP_ACCOUNT,
            tip_lamports: Self::MIN_TIP_LAMPORTS,
        }
    }

    pub fn fast(base_url: impl Into<String>, auth: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            auth: auth.into(),
            front_running_protection: false,
            submit_protection: SubmitProtection::Low,
            use_staked_rpcs: true,
            skip_preflight: true,
            tip_account: Self::DEFAULT_TIP_ACCOUNT,
            tip_lamports: Self::MIN_TIP_LAMPORTS,
        }
    }

    pub fn with_protection(mut self, level: SubmitProtection) -> Self {
        self.submit_protection = level;
        self
    }

    pub fn with_tip(mut self, account: Pubkey, lamports: u64) -> Self {
        self.tip_account = account;
        self.tip_lamports = lamports;
        self
    }
}

#[derive(Serialize)]
struct SubmitRequest<'a> {
    transaction: TxContent<'a>,
    #[serde(rename = "skipPreFlight")]
    skip_preflight: bool,
    #[serde(rename = "frontRunningProtection")]
    front_running_protection: bool,
    #[serde(rename = "submitProtection")]
    submit_protection: &'static str,
    #[serde(rename = "useStakedRPCs")]
    use_staked_rpcs: bool,
}

#[derive(Serialize)]
struct TxContent<'a> {
    content: &'a str,
}

#[derive(Deserialize)]
struct SubmitResponse {
    signature: String,
}

#[async_trait]
impl Submitter for BloxrouteSubmitter {
    async fn submit(&self, tx: &VersionedTransaction) -> Result<Signature> {
        let bytes = bincode::serialize(tx).map_err(|e| anyhow!("bloxroute: serialize tx: {e}"))?;
        let body = SubmitRequest {
            transaction: TxContent {
                content: &B64.encode(bytes),
            },
            skip_preflight: self.skip_preflight,
            front_running_protection: self.front_running_protection,
            submit_protection: self.submit_protection.as_str(),
            use_staked_rpcs: self.use_staked_rpcs,
        };
        let res = self
            .http
            .post(format!("{}/api/v2/submit", self.base_url))
            .header("Authorization", &self.auth)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                let cause = e
                    .source()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| e.to_string());
                anyhow!("bloxroute request (base_url={:?}): {cause}", self.base_url)
            })?;
        if !res.status().is_success() {
            let status = res.status();
            return Err(anyhow!(
                "bloxroute submit {status}: {}",
                res.text().await.unwrap_or_default()
            ));
        }
        let parsed: SubmitResponse = res.json().await?;
        parsed
            .signature
            .parse()
            .map_err(|_| anyhow!("bloxroute: bad signature {}", parsed.signature))
    }

    fn default_tip(&self) -> Option<Tip> {
        Some(Tip {
            account: self.tip_account,
            lamports: self.tip_lamports,
        })
    }
}
