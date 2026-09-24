use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_instruction::{AccountMeta, Instruction};

use crate::dexes::common::{COMPUTE_BUDGET_PROGRAM, WSOL};
use crate::error::{Result, TradeError};
use crate::lookup_table::load_address_lookup_tables;
use crate::{Dex, PreparedSwap, Pubkey, Quote, RpcClient, Settlement, Side, Trade, USDC_MINT};

const VENUE: &str = "dflow";
const QUOTE_PATH: &str = "/quote";
const INSTRUCTIONS_PATH: &str = "/swap-instructions";
const API_KEY_HEADER: &str = "x-api-key";
const TRANSACTION_VERSION: &str = "v0";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Atomic imperative swaps, including user-executed sponsorship; no intent/async orders.
pub struct DFlow {
    rpc: Arc<RpcClient>,
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl DFlow {
    pub fn new(rpc: Arc<RpcClient>, base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            rpc,
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
        }
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T> {
        let mut request = request.timeout(REQUEST_TIMEOUT);
        if let Some(key) = &self.api_key {
            request = request.header(API_KEY_HEADER, key);
        }
        let response = request.send().await.map_err(|source| TradeError::Network {
            context: VENUE,
            source,
        })?;
        if !response.status().is_success() {
            return Err(TradeError::Http {
                venue: VENUE,
                status: response.status().as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }
        response.json().await.map_err(decode_error)
    }

    async fn fetch_quote(&self, trade: &Trade, payer: Option<&Pubkey>) -> Result<(Value, Quote)> {
        let (input, output) = route_mints(trade);
        if trade.amount == 0 || trade.slippage_bps >= 10_000 || input == output {
            return Err(TradeError::Build(
                "DFlow requires different mints, positive input and slippage below 10000 bps"
                    .into(),
            ));
        }
        let mut request = self
            .http
            .get(format!("{}{QUOTE_PATH}", self.base_url))
            .query(&[
                ("inputMint", input.to_string()),
                ("outputMint", output.to_string()),
                ("amount", trade.amount.to_string()),
                ("slippageBps", trade.slippage_bps.to_string()),
                ("platformFeeBps", "0".into()),
                ("transactionVersion", TRANSACTION_VERSION.into()),
            ]);
        if payer.is_some() {
            request = request.query(&[("sponsoredSwap", true), ("sponsorExec", false)]);
        }
        let response: Value = self.request(request).await?;
        let quote = serde_json::from_value::<QuoteResponse>(response.clone())
            .map_err(decode_error)?
            .validated_quote(trade)?;
        Ok((response, quote))
    }

    async fn prepare_with_payer(
        &self,
        trade: &Trade,
        payer: Option<&Pubkey>,
    ) -> Result<PreparedSwap> {
        let (quote_response, quote) = self.fetch_quote(trade, payer).await?;
        let response: InstructionsResponse = self
            .request(
                self.http
                    .post(format!("{}{INSTRUCTIONS_PATH}", self.base_url))
                    .json(&InstructionsRequest {
                        quote_response,
                        user_public_key: trade.wallet.to_string(),
                        wrap_and_unwrap_sol: true,
                        transaction_version: TRANSACTION_VERSION,
                        dynamic_compute_unit_limit: false,
                        compute_unit_price_micro_lamports: 0,
                        sponsor: payer.map(ToString::to_string),
                        sponsor_exec: payer.map(|_| false),
                    }),
            )
            .await?;
        let instructions = response.instructions(trade.wallet, payer)?;
        let addresses = response
            .address_lookup_table_addresses
            .iter()
            .map(|address| address.parse().map_err(decode_error))
            .collect::<Result<Vec<_>>>()?;
        Ok(PreparedSwap {
            venue: VENUE,
            quote,
            instructions,
            lookup_tables: load_address_lookup_tables(&self.rpc, &addresses).await?,
        })
    }
}

#[async_trait]
impl Dex for DFlow {
    fn name(&self) -> &'static str {
        VENUE
    }

    async fn quote(&self, trade: &Trade) -> anyhow::Result<Quote> {
        Ok(self.fetch_quote(trade, None).await?.1)
    }

    async fn prepare_swap(&self, trade: &Trade) -> anyhow::Result<PreparedSwap> {
        Ok(self.prepare_with_payer(trade, None).await?)
    }

    async fn prepare_sponsored_swap(
        &self,
        trade: &Trade,
        payer: &Pubkey,
    ) -> anyhow::Result<PreparedSwap> {
        Ok(self.prepare_with_payer(trade, Some(payer)).await?)
    }
}

fn route_mints(trade: &Trade) -> (Pubkey, Pubkey) {
    let settlement = match trade.settlement {
        Settlement::Sol => WSOL,
        Settlement::Usdc => USDC_MINT,
    };
    match trade.side {
        Side::Buy => (settlement, trade.mint),
        Side::Sell => (trade.mint, settlement),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuoteResponse {
    input_mint: String,
    output_mint: String,
    in_amount: String,
    out_amount: String,
    min_out_amount: String,
    other_amount_threshold: String,
    slippage_bps: u64,
    platform_fee: Option<PlatformFee>,
    route_plan: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlatformFee {
    amount: String,
    fee_bps: u64,
}

impl QuoteResponse {
    fn validated_quote(&self, trade: &Trade) -> Result<Quote> {
        let (input, output) = route_mints(trade);
        let in_amount = self.in_amount.parse::<u64>().map_err(decode_error)?;
        let expected_out = self.out_amount.parse::<u64>().map_err(decode_error)?;
        let min_out = self
            .other_amount_threshold
            .parse::<u64>()
            .map_err(decode_error)?;
        if self.input_mint != input.to_string()
            || self.output_mint != output.to_string()
            || in_amount != trade.amount
            || self.slippage_bps != trade.slippage_bps
            || self.route_plan.is_empty()
        {
            return Err(decode_error("quote does not match the requested trade"));
        }
        let slippage_floor =
            u128::from(expected_out) * u128::from(10_000 - trade.slippage_bps) / 10_000;
        if min_out == 0
            || min_out > expected_out
            || u128::from(min_out) < slippage_floor
            || self.min_out_amount.parse::<u64>().map_err(decode_error)? != min_out
        {
            return Err(decode_error("invalid minimum output or slippage"));
        }
        if let Some(fee) = &self.platform_fee
            && (fee.fee_bps != 0 || fee.amount.parse::<u64>().map_err(decode_error)? != 0)
        {
            return Err(decode_error(
                "unexpected platform fee; SDK fee is applied separately",
            ));
        }
        Ok(Quote {
            in_amount,
            expected_out,
            min_out,
            // DFlow's provider impact is not the SDK's reserve-based, curve-only metric.
            price_impact_bps: None,
            fee: 0,
            application_fee: 0,
            sponsorship_fee: 0,
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InstructionsRequest {
    quote_response: Value,
    user_public_key: String,
    wrap_and_unwrap_sol: bool,
    transaction_version: &'static str,
    dynamic_compute_unit_limit: bool,
    compute_unit_price_micro_lamports: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    sponsor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sponsor_exec: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstructionsResponse {
    transaction_version: String,
    compute_budget_instructions: Vec<ApiInstruction>,
    setup_instructions: Vec<ApiInstruction>,
    swap_instruction: ApiInstruction,
    cleanup_instructions: Vec<ApiInstruction>,
    other_instructions: Vec<ApiInstruction>,
    address_lookup_table_addresses: Vec<String>,
}

impl InstructionsResponse {
    fn instructions(&self, wallet: Pubkey, payer: Option<&Pubkey>) -> Result<Vec<Instruction>> {
        if self.transaction_version != TRANSACTION_VERSION {
            return Err(decode_error("only v0 swap instructions are supported"));
        }
        let mut instructions = Vec::new();
        for instruction in self
            .compute_budget_instructions
            .iter()
            .chain(&self.setup_instructions)
            .chain(std::iter::once(&self.swap_instruction))
            .chain(&self.cleanup_instructions)
            .chain(&self.other_instructions)
        {
            let instruction = instruction.decode(wallet, payer)?;
            // The executor owns CU limit/price; retain heap and loaded-account budget requests.
            if instruction.program_id == COMPUTE_BUDGET_PROGRAM {
                match instruction.data.as_slice() {
                    [2, _, _, _, _] | [3, _, _, _, _, _, _, _, _] => continue,
                    [1 | 4, _, _, _, _] => {}
                    _ => return Err(decode_error("unsupported compute-budget instruction")),
                }
            }
            instructions.push(instruction);
        }
        if instructions.is_empty() || self.swap_instruction.data.is_empty() {
            return Err(decode_error("missing swap instruction"));
        }
        Ok(instructions)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiInstruction {
    program_id: String,
    accounts: Vec<ApiAccount>,
    data: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiAccount {
    pubkey: String,
    is_signer: bool,
    is_writable: bool,
}

impl ApiInstruction {
    fn decode(&self, wallet: Pubkey, payer: Option<&Pubkey>) -> Result<Instruction> {
        let accounts = self
            .accounts
            .iter()
            .map(|account| {
                let pubkey = account.pubkey.parse().map_err(decode_error)?;
                if account.is_signer && pubkey != wallet && Some(&pubkey) != payer {
                    return Err(decode_error("route requires an additional signer"));
                }
                Ok(AccountMeta {
                    pubkey,
                    is_signer: account.is_signer,
                    is_writable: account.is_writable,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Instruction {
            program_id: self.program_id.parse().map_err(decode_error)?,
            accounts,
            data: STANDARD.decode(&self.data).map_err(decode_error)?,
        })
    }
}

fn decode_error(error: impl std::fmt::Display) -> TradeError {
    TradeError::Decode(VENUE, error.to_string())
}

#[cfg(test)]
#[path = "../tests/unit/dflow.rs"]
mod tests;
