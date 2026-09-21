use std::collections::HashMap;
use std::error::Error as _;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::Deserialize;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::AddressLookupTableAccount;
use solana_pubkey::Pubkey;

use crate::dexes::common::WSOL;
use crate::error::{Result, TradeError};
use crate::types::{Dex, PreparedSwap, Quote, Settlement, Side, Trade};

#[cfg(test)]
mod tests;

pub struct Jupiter {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildResponse {
    input_mint: String,
    output_mint: String,
    swap_mode: String,
    in_amount: String,
    out_amount: String,
    other_amount_threshold: String,
    setup_instructions: Vec<ApiInstruction>,
    swap_instruction: ApiInstruction,
    cleanup_instruction: Option<ApiInstruction>,
    other_instructions: Vec<ApiInstruction>,
    addresses_by_lookup_table_address: Option<HashMap<String, Vec<String>>>,
}

impl BuildResponse {
    fn validate(&self, trade: &Trade) -> Result<()> {
        let (input, output) = Jupiter::route_mints(trade);
        if self.swap_mode != "ExactIn"
            || self.input_mint != input.to_string()
            || self.output_mint != output.to_string()
            || parse_amount("inAmount", &self.in_amount)? != trade.amount
        {
            return Err(TradeError::Decode(
                "jupiter",
                "build response does not match the requested exact-input trade".into(),
            ));
        }
        let quote = self.quote()?;
        if quote.min_out == 0 || quote.expected_out < quote.min_out {
            return Err(TradeError::Decode(
                "jupiter",
                "invalid minimum output".into(),
            ));
        }
        Ok(())
    }

    fn quote(&self) -> Result<Quote> {
        Ok(Quote {
            in_amount: parse_amount("inAmount", &self.in_amount)?,
            // This adapter has no pool-reserve snapshot for the SDK's curve-only metric.
            price_impact_bps: None,
            expected_out: parse_amount("outAmount", &self.out_amount)?,
            min_out: parse_amount("otherAmountThreshold", &self.other_amount_threshold)?,
            fee: 0,
            application_fee: 0,
        })
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

impl Jupiter {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key,
        }
    }

    fn route_mints(p: &Trade) -> (Pubkey, Pubkey) {
        let settlement_mint = match p.settlement {
            Settlement::Sol => WSOL,
            Settlement::Usdc => crate::USDC_MINT,
        };
        match p.side {
            Side::Buy => (settlement_mint, p.mint),
            Side::Sell => (p.mint, settlement_mint),
        }
    }

    fn send_err(&self, e: reqwest::Error) -> TradeError {
        if e.is_builder() {
            let detail = e
                .source()
                .map(|s| s.to_string())
                .unwrap_or_else(|| e.to_string());
            TradeError::Config {
                what: "jupiter",
                url: self.base_url.clone(),
                detail,
            }
        } else {
            TradeError::Network {
                context: "jupiter /build",
                source: e,
            }
        }
    }

    async fn build(&self, p: &Trade) -> Result<BuildResponse> {
        let (input_mint, output_mint) = Self::route_mints(p);
        if p.amount == 0 || p.slippage_bps >= 10_000 || input_mint == output_mint {
            return Err(TradeError::Build(
                "Jupiter requires different mints, positive input and slippage below 10000 bps"
                    .into(),
            ));
        }
        let mut req = self
            .http
            .get(format!("{}/swap/v2/build", self.base_url))
            .query(&[
                ("inputMint", input_mint.to_string()),
                ("outputMint", output_mint.to_string()),
                ("amount", p.amount.to_string()),
                ("taker", p.wallet.to_string()),
                ("slippageBps", p.slippage_bps.to_string()),
                ("wrapAndUnwrapSol", "true".into()),
            ]);
        if let Some(key) = &self.api_key {
            req = req.header("x-api-key", key);
        }
        let res = req.send().await.map_err(|e| self.send_err(e))?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            return Err(TradeError::Http {
                venue: "jupiter",
                status,
                body: res.text().await.unwrap_or_default(),
            });
        }
        let build = res
            .json::<BuildResponse>()
            .await
            .map_err(|e| TradeError::Decode("jupiter", e.to_string()))?;
        build.validate(p)?;
        Ok(build)
    }

    fn swap_instructions(b: &BuildResponse) -> Result<Vec<Instruction>> {
        let mut ixs = Vec::new();
        for ix in &b.setup_instructions {
            ixs.push(to_instruction(ix)?);
        }
        ixs.push(to_instruction(&b.swap_instruction)?);
        if let Some(c) = &b.cleanup_instruction {
            ixs.push(to_instruction(c)?);
        }
        for ix in &b.other_instructions {
            ixs.push(to_instruction(ix)?);
        }
        Ok(ixs)
    }

    fn lookup_tables(b: &BuildResponse) -> Result<Vec<AddressLookupTableAccount>> {
        let Some(map) = &b.addresses_by_lookup_table_address else {
            return Ok(vec![]);
        };
        map.iter()
            .map(|(key, addrs)| {
                Ok(AddressLookupTableAccount {
                    key: key
                        .parse()
                        .map_err(|_| TradeError::Decode("jupiter", format!("bad ALT key {key}")))?,
                    addresses: addrs
                        .iter()
                        .map(|a| {
                            a.parse().map_err(|_| {
                                TradeError::Decode("jupiter", format!("bad ALT addr {a}"))
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                })
            })
            .collect()
    }
}

#[async_trait]
impl Dex for Jupiter {
    fn name(&self) -> &'static str {
        "jupiter"
    }

    async fn quote(&self, p: &Trade) -> anyhow::Result<Quote> {
        Ok(self.build(p).await?.quote()?)
    }

    async fn prepare_swap(&self, p: &Trade) -> anyhow::Result<PreparedSwap> {
        let build = self.build(p).await?;
        let instructions = Self::swap_instructions(&build)?;
        let alts = Self::lookup_tables(&build)?;
        Ok(PreparedSwap {
            venue: self.name(),
            quote: build.quote()?,
            instructions,
            lookup_tables: alts,
        })
    }
}

fn parse_amount(field: &str, s: &str) -> Result<u64> {
    s.parse()
        .map_err(|_| TradeError::Decode("jupiter", format!("bad {field} {s:?}")))
}

fn to_instruction(ix: &ApiInstruction) -> Result<Instruction> {
    Ok(Instruction {
        program_id: ix.program_id.parse().map_err(|_| {
            TradeError::Decode("jupiter", format!("bad program id {}", ix.program_id))
        })?,
        accounts: ix
            .accounts
            .iter()
            .map(|a| {
                Ok(AccountMeta {
                    pubkey: a.pubkey.parse().map_err(|_| {
                        TradeError::Decode("jupiter", format!("bad pubkey {}", a.pubkey))
                    })?,
                    is_signer: a.is_signer,
                    is_writable: a.is_writable,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        data: B64
            .decode(&ix.data)
            .map_err(|e| TradeError::Decode("jupiter", format!("bad instruction data: {e}")))?,
    })
}
