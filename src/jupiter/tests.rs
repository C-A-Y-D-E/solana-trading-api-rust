use super::*;
use crate::dexes::common::{
    TOKEN_PROGRAM, ata, close_account, create_ata_idempotent, system_transfer,
};
use crate::{RpcClient, SdkFee, TradingClient, USDC_MINT};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn trade(side: Side, settlement: Settlement) -> Trade {
    Trade {
        side,
        ..Trade::buy(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            1_000_000,
            100,
            None,
        )
        .with_settlement(settlement)
    }
}

fn instruction_json(instruction: &Instruction) -> Value {
    json!({
        "programId": instruction.program_id.to_string(),
        "accounts": instruction.accounts.iter().map(|account| json!({
            "pubkey": account.pubkey.to_string(),
            "isSigner": account.is_signer,
            "isWritable": account.is_writable,
        })).collect::<Vec<_>>(),
        "data": B64.encode(&instruction.data),
    })
}

fn build_response(trade: &Trade) -> Value {
    let (input, output) = Jupiter::route_mints(trade);
    let marker = Instruction {
        program_id: Pubkey::new_unique(),
        accounts: vec![AccountMeta::new(trade.mint, false)],
        data: vec![1, 2, 3],
    };
    let cleanup = (trade.settlement == Settlement::Sol).then(|| {
        instruction_json(&close_account(
            &ata(&trade.wallet, &WSOL, &TOKEN_PROGRAM),
            &trade.wallet,
            &trade.wallet,
        ))
    });
    json!({
        "inputMint": input.to_string(), "outputMint": output.to_string(),
        "swapMode": "ExactIn", "inAmount": trade.amount.to_string(),
        "outAmount": "100000", "otherAmountThreshold": "90000",
        "setupInstructions": [instruction_json(&marker)],
        "swapInstruction": instruction_json(&marker),
        "cleanupInstruction": cleanup,
        "otherInstructions": [instruction_json(&marker)],
        "addressesByLookupTableAddress": {
            Pubkey::new_unique().to_string(): [trade.mint.to_string()]
        },
    })
}

async fn mock_build_server(
    response: Value,
    requests: usize,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async move {
            let mut captured = Vec::new();
            for _ in 0..requests {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                    let mut buffer = [0; 1024];
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert!(read > 0, "incomplete HTTP request");
                    request.extend_from_slice(&buffer[..read]);
                }
                captured.push(String::from_utf8(request).unwrap());
                let body = response.to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            captured
        }).await.expect("mock Jupiter requests timed out")
    });
    (url, task)
}

fn assert_request_matches_trade(request: &str, trade: &Trade) {
    let path = request.split_whitespace().nth(1).unwrap();
    let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
    assert_eq!(url.path(), "/swap/v2/build");
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    let (input, output) = Jupiter::route_mints(trade);
    assert_eq!(query["inputMint"], input.to_string());
    assert_eq!(query["outputMint"], output.to_string());
    assert_eq!(query["amount"], trade.amount.to_string());
    assert_eq!(query["taker"], trade.wallet.to_string());
    assert_eq!(query["wrapAndUnwrapSol"], "true");
    assert!(
        !query.contains_key("platformFeeBps"),
        "SDK fee must not also be charged by Jupiter"
    );
    assert!(!query.contains_key("feeAccount"));
}

fn assert_fee_instructions(prepared: &PreparedSwap, trade: &Trade, recipient: Pubkey) {
    let amount = prepared.quote.application_fee;
    match trade.settlement {
        Settlement::Sol => assert_eq!(
            prepared.instructions.last().unwrap(),
            &system_transfer(&trade.wallet, &recipient, amount),
        ),
        Settlement::Usdc => {
            let tail = &prepared.instructions[prepared.instructions.len() - 2..];
            assert_eq!(
                tail[0],
                create_ata_idempotent(&trade.wallet, &recipient, &USDC_MINT, &TOKEN_PROGRAM)
            );
            assert_eq!(
                tail[1],
                spl_token::instruction::transfer_checked(
                    &TOKEN_PROGRAM,
                    &ata(&trade.wallet, &USDC_MINT, &TOKEN_PROGRAM),
                    &USDC_MINT,
                    &ata(&recipient, &USDC_MINT, &TOKEN_PROGRAM),
                    &trade.wallet,
                    &[],
                    amount,
                    6,
                )
                .unwrap()
            );
        }
    }
}

#[tokio::test]
async fn client_jupiter_quotes_and_prepares_one_fee_for_each_side_and_settlement() {
    for settlement in [Settlement::Sol, Settlement::Usdc] {
        for side in [Side::Buy, Side::Sell] {
            let trade = trade(side, settlement);
            let recipient = Pubkey::new_unique();
            let fee = SdkFee::new(recipient, 100).unwrap();
            let adjusted = fee.venue_trade(&trade).unwrap();
            let response = build_response(&adjusted);
            let build: BuildResponse = serde_json::from_value(response.clone()).unwrap();
            let swap_instructions = Jupiter::swap_instructions(&build).unwrap();
            let (url, requests) = mock_build_server(response, 2).await;
            let client =
                TradingClient::new(Arc::new(RpcClient::new_mock("fails".into())), url, None)
                    .with_sdk_fee(fee);

            let quote = client.quote(&trade).await.unwrap();
            let prepared = client.prepare_swap(&trade).await.unwrap();

            let (expected_fee, expected_out, min_out) = match side {
                Side::Buy => (10_000, 100_000, 90_000),
                Side::Sell => (900, 99_100, 89_100),
            };
            for quote in [quote, prepared.quote] {
                assert_eq!(quote.in_amount, trade.amount);
                assert_eq!(quote.application_fee, expected_fee);
                assert_eq!((quote.expected_out, quote.min_out), (expected_out, min_out));
            }
            assert_eq!(prepared.venue, "jupiter");
            assert_eq!(
                &prepared.instructions[..swap_instructions.len()],
                &swap_instructions
            );
            assert_eq!(
                prepared.instructions.len(),
                swap_instructions.len() + if settlement == Settlement::Sol { 1 } else { 2 }
            );
            assert_fee_instructions(&prepared, &trade, recipient);
            assert_eq!(prepared.lookup_tables.len(), 1);
            assert_eq!(prepared.lookup_tables[0].addresses, vec![trade.mint]);
            for request in requests.await.unwrap() {
                assert_request_matches_trade(&request, &adjusted);
            }
        }
    }
}

#[tokio::test]
async fn jupiter_without_fee_and_zero_rate_keep_original_instructions() {
    for settlement in [Settlement::Sol, Settlement::Usdc] {
        let trade = trade(Side::Buy, settlement);
        let response = build_response(&trade);
        let build: BuildResponse = serde_json::from_value(response.clone()).unwrap();
        let expected = Jupiter::swap_instructions(&build).unwrap();
        let (url, requests) = mock_build_server(response, 2).await;
        let client = TradingClient::new(Arc::new(RpcClient::new_mock("fails".into())), url, None);
        assert_eq!(
            client.prepare_swap(&trade).await.unwrap().instructions,
            expected
        );
        let client = client.with_sdk_fee(SdkFee::new(Pubkey::new_unique(), 0).unwrap());
        let prepared = client.prepare_swap(&trade).await.unwrap();
        assert_eq!(prepared.instructions, expected);
        assert_eq!(prepared.quote.application_fee, 0);
        for request in requests.await.unwrap() {
            assert_request_matches_trade(&request, &trade);
        }
    }
}

#[test]
fn build_response_rejects_mismatched_routes_and_invalid_amounts() {
    let trade = trade(Side::Buy, Settlement::Sol);
    for (field, value) in [
        ("inputMint", Pubkey::new_unique().to_string()),
        ("outputMint", Pubkey::new_unique().to_string()),
        ("swapMode", "ExactOut".into()),
        ("inAmount", "999999".into()),
        ("inAmount", "invalid".into()),
        ("otherAmountThreshold", "0".into()),
        ("otherAmountThreshold", "100001".into()),
    ] {
        let mut response = build_response(&trade);
        response[field] = json!(value);
        let build: BuildResponse = serde_json::from_value(response).unwrap();
        assert!(build.validate(&trade).is_err(), "accepted invalid {field}");
    }
}

#[tokio::test]
async fn malformed_jupiter_response_fails_before_fee_preparation() {
    let trade = trade(Side::Sell, Settlement::Usdc);
    let mut response = build_response(&trade);
    response["swapMode"] = json!("ExactOut");
    let (url, requests) = mock_build_server(response, 1).await;
    let client = TradingClient::new(Arc::new(RpcClient::new_mock("fails".into())), url, None)
        .with_sdk_fee(SdkFee::new(Pubkey::new_unique(), 100).unwrap());
    assert!(
        client
            .prepare_swap(&trade)
            .await
            .unwrap_err()
            .to_string()
            .contains("exact-input")
    );
    requests.await.unwrap();
}
