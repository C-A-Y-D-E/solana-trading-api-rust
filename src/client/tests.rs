mod live;

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestDex {
    fail: bool,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Dex for TestDex {
    fn name(&self) -> &'static str {
        "fixture"
    }
    async fn quote(&self, _: &Trade) -> anyhow::Result<Quote> {
        panic!("preparation must not fetch a separate quote")
    }
    async fn prepare_swap(&self, trade: &Trade) -> anyhow::Result<PreparedSwap> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail, "route unavailable");
        Ok(PreparedSwap {
            venue: self.name(),
            quote: Quote {
                in_amount: trade.amount,
                price_impact_bps: None,
                expected_out: 1_000,
                min_out: 900,
                fee: 10,
                application_fee: 0,
            },
            instructions: vec![crate::dexes::common::system_transfer(
                &trade.wallet,
                &Pubkey::new_unique(),
                1,
            )],
            lookup_tables: vec![],
        })
    }
}

fn test_trade() -> Trade {
    Trade::buy(
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        1_000,
        100,
        Some(Venue::PumpFun),
    )
}

#[tokio::test]
async fn route_preparation_can_fallback_but_fee_mode_cannot() {
    let primary = TestDex {
        fail: true,
        calls: AtomicUsize::new(0),
    };
    let fallback = TestDex {
        fail: false,
        calls: AtomicUsize::new(0),
    };
    let trade = test_trade();
    prepare_trade(&trade, &primary, Some(&fallback), None)
        .await
        .unwrap();
    assert_eq!(fallback.calls.load(Ordering::SeqCst), 1);
    let fee = SdkFee::new(Pubkey::new_unique(), 100).unwrap();
    assert!(
        prepare_trade(&trade, &primary, Some(&fallback), Some(fee))
            .await
            .is_err()
    );
    assert_eq!(fallback.calls.load(Ordering::SeqCst), 1);
    assert!(prepare_trade(&trade, &primary, None, None).await.is_err());
}

#[tokio::test]
async fn signing_and_submission_errors_never_reprepare_or_fallback() {
    use solana_transaction::versioned::VersionedTransaction;
    struct TestSigner {
        reject: bool,
    }
    #[async_trait::async_trait]
    impl Signer for TestSigner {
        async fn sign(&self, _: &Pubkey, _: &VersionedTransaction) -> anyhow::Result<Signature> {
            anyhow::ensure!(!self.reject, "signing rejected");
            Ok(Signature::default())
        }
    }
    struct AmbiguousSubmitter(AtomicUsize);
    #[async_trait::async_trait]
    impl Submitter for AmbiguousSubmitter {
        async fn submit(&self, _: &VersionedTransaction) -> anyhow::Result<Signature> {
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("timeout after accepting transaction")
        }
    }
    for reject in [true, false] {
        let primary = TestDex {
            fail: false,
            calls: AtomicUsize::new(0),
        };
        let fallback = TestDex {
            fail: false,
            calls: AtomicUsize::new(0),
        };
        let trade = test_trade();
        let prepared = prepare_trade(&trade, &primary, Some(&fallback), None)
            .await
            .unwrap();
        let rpc = Arc::new(RpcClient::new_mock("succeeds".into()));
        let submitter = AmbiguousSubmitter(AtomicUsize::new(0));
        let error = submit_swap(
            &rpc,
            prepared,
            &TestSigner { reject },
            &submitter,
            &trade,
            0,
            &[],
        )
        .await
        .unwrap_err();
        if reject {
            assert!(matches!(error, TradeError::Sign(_)));
        } else {
            assert!(matches!(error, TradeError::Submit(_)));
        }
        assert_eq!(submitter.0.load(Ordering::SeqCst), usize::from(!reject));
        assert_eq!(primary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn pumpswap_never_falls_back_to_jupiter() {
    assert!(!allows_jupiter_fallback(Some(Venue::PumpSwap)));
    assert!(allows_jupiter_fallback(Some(Venue::PumpFun)));
    let client = TradingClient::new(
        Arc::new(RpcClient::new_mock("fails".into())),
        "unused",
        None,
    )
    .with_sdk_fee(SdkFee::new(Pubkey::new_unique(), 100).unwrap());
    assert!(client.fallback(Some(Venue::PumpFun)).is_none());
}

#[tokio::test]
async fn invalid_usdc_aggregator_amount_is_rejected_without_network_access() {
    let client = TradingClient::new(
        Arc::new(RpcClient::new_mock("fails".into())),
        "https://unused.invalid",
        None,
    );
    let trade = Trade::buy(
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        1_000,
        100,
        Some(Venue::PumpFun),
    )
    .with_settlement(Settlement::Usdc);
    let trade = Trade {
        venue: None,
        amount: 0,
        ..trade
    };
    assert!(
        client
            .quote(&trade)
            .await
            .unwrap_err()
            .to_string()
            .contains("positive input")
    );
}

#[tokio::test]
async fn sdk_fee_mode_rejects_zero_aggregator_amount_without_network_access() {
    let client = TradingClient::new(
        Arc::new(RpcClient::new_mock("fails".into())),
        "https://unused.invalid",
        None,
    )
    .with_sdk_fee(SdkFee::new(Pubkey::new_unique(), 100).unwrap());
    let trade = Trade::buy(Pubkey::new_unique(), Pubkey::new_unique(), 0, 100, None);
    assert!(
        client
            .quote(&trade)
            .await
            .unwrap_err()
            .to_string()
            .contains("amount must be positive")
    );
}

#[test]
fn shared_tables_are_deduplicated_replaced_and_retained_in_fee_mode() {
    let rpc = Arc::new(RpcClient::new_mock("succeeds".into()));
    let table = AddressLookupTableAccount {
        key: Pubkey::new_unique(),
        addresses: vec![Pubkey::new_unique()],
    };
    let client = TradingClient::new(rpc, "https://unused.invalid", None);
    assert!(client.shared_lookup_tables().is_empty());
    let client = client
        .with_shared_lookup_tables(vec![table.clone(), table.clone()])
        .with_sdk_fee(SdkFee::new(Pubkey::new_unique(), 100).unwrap());
    assert_eq!(client.shared_lookup_tables().len(), 1);
    assert_eq!(client.shared_lookup_tables()[0].key, table.key);
    assert!(
        client
            .with_shared_lookup_tables(vec![])
            .shared_lookup_tables()
            .is_empty()
    );
}

#[tokio::test]
async fn failed_refresh_preserves_shared_snapshots() {
    let rpc = Arc::new(RpcClient::new_mock("fails".into()));
    let table = AddressLookupTableAccount {
        key: Pubkey::new_unique(),
        addresses: vec![Pubkey::new_unique()],
    };
    let mut client = TradingClient::new(rpc, "https://unused.invalid", None)
        .with_shared_lookup_tables(vec![table.clone()]);
    assert!(client.refresh_shared_lookup_tables().await.is_err());
    assert_eq!(client.shared_lookup_tables()[0].key, table.key);
    assert_eq!(client.shared_lookup_tables()[0].addresses, table.addresses);
}

#[tokio::test]
async fn empty_shared_address_configuration_does_not_need_rpc() {
    let rpc = Arc::new(RpcClient::new_mock("fails".into()));
    let mut client = TradingClient::new(rpc, "https://unused.invalid", None)
        .with_shared_lookup_table_addresses(&[])
        .await
        .unwrap();
    assert!(client.shared_lookup_tables().is_empty());
    client.refresh_shared_lookup_tables().await.unwrap();
}
