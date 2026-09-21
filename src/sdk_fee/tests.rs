use super::*;
use crate::client::prepare_trade;
use crate::dexes::common::{TOKEN_PROGRAM, WSOL, ata, close_account};
use crate::types::Dex;
use crate::types::Venue;
use async_trait::async_trait;
use solana_instruction::Instruction;
use solana_message::AddressLookupTableAccount;

fn fee() -> SdkFee {
    SdkFee::new(Pubkey::new_unique(), 100).unwrap()
}

fn trade(side: Side) -> Trade {
    Trade {
        side,
        ..Trade::buy(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            1_000_000,
            100,
            Some(Venue::PumpSwap),
        )
    }
}

fn quote() -> Quote {
    Quote {
        in_amount: 1_000_000,
        price_impact_bps: Some(250.0),
        expected_out: 200_000,
        min_out: 190_000,
        fee: 42,
        application_fee: 0,
    }
}

#[test]
fn configuration_rejects_invalid_rates_and_recipient() {
    assert!(SdkFee::new(Pubkey::default(), 100).is_err());
    assert!(SdkFee::new(Pubkey::new_unique(), 10_000).is_err());
    assert!(SdkFee::new(Pubkey::new_unique(), u16::MAX).is_err());
    assert!(SdkFee::new(Pubkey::new_unique(), 0).is_ok());
    assert!(SdkFee::new(Pubkey::new_unique(), 9_999).is_ok());
}

#[test]
fn fees_round_down_without_overflow_or_minimum_charge() {
    let fee = fee();
    assert_eq!(fee.amount(99), 0);
    assert_eq!(fee.amount(100), 1);
    assert_eq!(fee.amount(u64::MAX), u64::MAX / 100);
    assert_eq!(fee.amount(1_000_000_000), 10_000_000);
}

#[test]
fn buy_reserves_fee_from_gross_input_without_changing_request() {
    let fee = fee();
    let trade = trade(Side::Buy);
    assert_eq!(fee.venue_trade(&trade).unwrap().amount, 990_000);
    assert_eq!(trade.amount, 1_000_000);
    let net = fee.net_quote(&trade, quote()).unwrap();
    assert_eq!((net.in_amount, net.application_fee), (1_000_000, 10_000));
    assert_eq!((net.expected_out, net.min_out), (200_000, 190_000));
}

#[test]
fn sell_fee_uses_minimum_not_expected_output() {
    let fee = fee();
    let trade = trade(Side::Sell);
    assert_eq!(fee.venue_trade(&trade).unwrap().amount, trade.amount);
    let net = fee.net_quote(&trade, quote()).unwrap();
    assert_eq!(net.application_fee, 1_900);
    assert_eq!((net.expected_out, net.min_out), (198_100, 188_100));
}

#[test]
fn application_fee_does_not_get_added_to_price_impact() {
    for side in [Side::Buy, Side::Sell] {
        let net = fee().net_quote(&trade(side), quote()).unwrap();
        assert_eq!(net.price_impact_bps, Some(250.0));
        let unknown = Quote {
            price_impact_bps: None,
            ..quote()
        };
        assert_eq!(
            fee()
                .net_quote(&trade(side), unknown)
                .unwrap()
                .price_impact_bps,
            None
        );
    }
}

#[tokio::test]
async fn usdc_fees_create_recipient_ata_and_use_checked_token_transfer() {
    for side in [Side::Buy, Side::Sell] {
        let fee = fee();
        let trade = trade(side).with_settlement(Settlement::Usdc);
        let prepared = prepare_trade(&trade, &SnapshotDex, None, Some(fee))
            .await
            .unwrap();
        let expected_fee = if side == Side::Buy { 10_000 } else { 1_900 };
        assert_eq!(prepared.quote.application_fee, expected_fee);
        assert_eq!(prepared.quote.fee, 42);
        assert_eq!(prepared.instructions.len(), 5);
        assert_eq!(
            prepared.instructions[3],
            crate::dexes::common::create_ata_idempotent(
                &trade.wallet,
                &fee.recipient(),
                &USDC_MINT,
                &TOKEN_PROGRAM
            )
        );
        let transfer = &prepared.instructions[4];
        assert_eq!(transfer.program_id, TOKEN_PROGRAM);
        assert_eq!(
            transfer.accounts[0].pubkey,
            ata(&trade.wallet, &USDC_MINT, &TOKEN_PROGRAM)
        );
        assert_eq!(transfer.accounts[1].pubkey, USDC_MINT);
        assert_eq!(
            transfer.accounts[2].pubkey,
            ata(&fee.recipient(), &USDC_MINT, &TOKEN_PROGRAM)
        );
        assert_eq!(transfer.accounts[3].pubkey, trade.wallet);
        assert!(transfer.accounts[3].is_signer);
        assert_eq!(
            spl_token::instruction::TokenInstruction::unpack(&transfer.data).unwrap(),
            spl_token::instruction::TokenInstruction::TransferChecked {
                amount: expected_fee,
                decimals: 6
            }
        );
    }
}

#[tokio::test]
async fn zero_usdc_fee_does_not_create_ata_or_transfer() {
    let trade = trade(Side::Sell).with_settlement(Settlement::Usdc);
    let fee = SdkFee::new(Pubkey::new_unique(), 0).unwrap();
    let prepared = prepare_trade(&trade, &SnapshotDex, None, Some(fee))
        .await
        .unwrap();
    assert_eq!(prepared.instructions.len(), 3);
    assert_eq!(prepared.quote.application_fee, 0);
}

#[test]
fn invalid_trades_and_zero_minimum_fail_closed() {
    let fee = fee();
    let valid = trade(Side::Sell);
    for invalid in [
        Trade { amount: 0, ..valid },
        Trade {
            slippage_bps: 10_000,
            ..valid
        },
        Trade {
            wallet: fee.recipient(),
            ..valid
        },
    ] {
        assert!(fee.venue_trade(&invalid).is_err());
    }
    assert!(
        fee.net_quote(
            &valid,
            Quote {
                min_out: 0,
                ..quote()
            }
        )
        .is_err()
    );
    assert!(
        fee.net_quote(
            &valid,
            Quote {
                expected_out: 1,
                ..quote()
            }
        )
        .is_err()
    );
}

struct SnapshotDex;

#[async_trait]
impl Dex for SnapshotDex {
    fn name(&self) -> &'static str {
        "fixture"
    }

    async fn quote(&self, _: &Trade) -> anyhow::Result<Quote> {
        Ok(Quote {
            expected_out: 800_000,
            min_out: 700_000,
            ..quote()
        })
    }

    async fn swap(
        &self,
        _: &Trade,
    ) -> anyhow::Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
        panic!("fee path must use snapshot-consistent preparation")
    }

    async fn prepare_swap(&self, trade: &Trade) -> anyhow::Result<PreparedSwap> {
        let token_account = ata(&trade.wallet, &WSOL, &TOKEN_PROGRAM);
        let hop = Instruction {
            program_id: crate::dexes::pumpswap::PROGRAM_ID,
            accounts: vec![],
            data: trade.amount.to_le_bytes().to_vec(),
        };
        Ok(PreparedSwap {
            venue: self.name(),
            quote: quote(),
            instructions: vec![
                hop.clone(),
                hop,
                close_account(&token_account, &trade.wallet, &trade.wallet),
            ],
            lookup_tables: vec![AddressLookupTableAccount {
                key: WSOL,
                addresses: vec![token_account],
            }],
        })
    }
}

#[tokio::test]
async fn sell_charges_once_after_two_hops_and_unwrap_using_prepared_minimum() {
    let fee = fee();
    let trade = trade(Side::Sell);
    let preview = fee
        .net_quote(&trade, SnapshotDex.quote(&trade).await.unwrap())
        .unwrap();
    assert_eq!(preview.application_fee, 7_000);
    assert_eq!(preview.fee, 42);
    let prepared = prepare_trade(&trade, &SnapshotDex, None, Some(fee))
        .await
        .unwrap();
    assert_eq!(prepared.quote.fee, 42);
    assert_eq!(prepared.quote.application_fee, 1_900);
    assert_eq!(prepared.instructions.len(), 4);
    assert_eq!(prepared.instructions[2].program_id, TOKEN_PROGRAM);
    assert_eq!(prepared.instructions[2].data, vec![9]);
    assert_eq!(
        prepared.instructions[3],
        system_transfer(&trade.wallet, &fee.recipient(), 1_900)
    );
    assert_eq!(prepared.lookup_tables[0].key, WSOL);
}

#[tokio::test]
async fn buy_prepares_only_net_input_and_charges_one_fee() {
    let fee = fee();
    let trade = trade(Side::Buy);
    let prepared = prepare_trade(&trade, &SnapshotDex, None, Some(fee))
        .await
        .unwrap();
    assert_eq!(prepared.quote.fee, 42);
    assert_eq!(prepared.instructions[0].data, 990_000_u64.to_le_bytes());
    assert_eq!(
        prepared.instructions[3],
        system_transfer(&trade.wallet, &fee.recipient(), 10_000)
    );
    assert_eq!(prepared.quote.in_amount, 1_000_000);
}

#[tokio::test]
async fn zero_rate_and_rounded_zero_do_not_add_transfers() {
    let zero_fee = SdkFee::new(Pubkey::new_unique(), 0).unwrap();
    let prepared = prepare_trade(&trade(Side::Sell), &SnapshotDex, None, Some(zero_fee))
        .await
        .unwrap();
    assert_eq!(prepared.instructions.len(), 3);
    assert_eq!(prepared.quote.application_fee, 0);
    assert_eq!(prepared.quote.min_out, quote().min_out);

    let prepared = prepare_trade(
        &Trade {
            amount: 1,
            ..trade(Side::Buy)
        },
        &SnapshotDex,
        None,
        Some(fee()),
    )
    .await
    .unwrap();
    assert_eq!(prepared.instructions.len(), 3);
    assert_eq!(prepared.quote.application_fee, 0);
}

#[tokio::test]
async fn executor_compiles_fee_and_swap_together_with_shared_lookup_table() {
    use crate::types::{Signer, Submitter, SwapStatus};
    use solana_client::nonblocking::rpc_client::RpcClient;
    use solana_message::VersionedMessage;
    use solana_signature::Signature;
    use solana_transaction::versioned::VersionedTransaction;
    use std::sync::Arc;

    struct InspectTransaction;

    fn inspect(tx: &VersionedTransaction) {
        let VersionedMessage::V0(message) = &tx.message else {
            panic!("expected v0")
        };
        assert_eq!(message.instructions.len(), 6);
        assert_eq!(message.address_table_lookups.len(), 2);
        let fee = message.instructions.last().unwrap();
        assert_eq!(
            message.account_keys[usize::from(fee.program_id_index)],
            crate::dexes::common::SYSTEM_PROGRAM
        );
        assert_eq!(&fee.data[..4], &2_u32.to_le_bytes());
        assert_eq!(&fee.data[4..], &1_900_u64.to_le_bytes());
        assert!(usize::from(fee.accounts[1]) >= message.account_keys.len());
    }

    #[async_trait]
    impl Signer for InspectTransaction {
        async fn sign(&self, _: &Pubkey, tx: &VersionedTransaction) -> anyhow::Result<Signature> {
            inspect(tx);
            Ok(Signature::default())
        }
    }

    #[async_trait]
    impl Submitter for InspectTransaction {
        async fn submit(&self, tx: &VersionedTransaction) -> anyhow::Result<Signature> {
            inspect(tx);
            Ok(Signature::default())
        }
    }

    let fee = fee();
    let trade = trade(Side::Sell);
    let table = AddressLookupTableAccount {
        key: Pubkey::new_unique(),
        addresses: vec![ata(&trade.wallet, &WSOL, &TOKEN_PROGRAM), fee.recipient()],
    };
    let rpc = Arc::new(RpcClient::new_mock("succeeds".into()));
    let result = crate::executor::submit_swap(
        &rpc,
        prepare_trade(&trade, &SnapshotDex, None, Some(fee))
            .await
            .unwrap(),
        &InspectTransaction,
        &InspectTransaction,
        &trade,
        0,
        &[table],
    )
    .await
    .unwrap();
    assert_eq!(result.status, SwapStatus::Pending);
}
