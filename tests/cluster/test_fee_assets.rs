//! QQQ/USD-style asset metadata and quote-volume fee deduction.
//!
//! One process trades one ETF/fiat pair: fractional-share lots, a cent tick,
//! quote-volume limit normalization and fees charged in the quote asset.

use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::match_result::MatchEventKind;
use matchengine_raft::order_book::AssetClass;
use matchengine_raft::order_book::AssetId;
use matchengine_raft::order_book::FeeAsset;
use matchengine_raft::order_book::FeeAssetPolicy;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::order_book::RejectReason;
use matchengine_raft::order_book::SymbolConfig;
use matchengine_raft::order_book::VolumeUnit;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21902";
const BTC_ADDR: &str = "127.0.0.1:21924";

fn fixed(value: &str) -> Fixed {
    value.parse().expect("valid exact decimal")
}

#[test]
fn etf_fiat_quote_volume_and_quote_fee_deduction() {
    C::run(async {
        let temp = TempDir::new("fee-assets");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("fee-assets");

        let node =
            build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage.clone())
                .await
                .unwrap();
        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        let config = SymbolConfig::new(0, 2, 4)
            .unwrap()
            .with_assets(
                AssetId::new("QQQ", AssetClass::Etf, 4).unwrap(),
                AssetId::new("USD", AssetClass::Fiat, 2).unwrap(),
            )
            .unwrap()
            .with_tick_and_lot(fixed("0.01"), fixed("0.0001"))
            .with_fee_asset_policy(FeeAssetPolicy::Quote);
        let configured = client
            .write(&MatchEngineRequest::ConfigureSymbol {
                config: config.clone(),
            })
            .await
            .unwrap()
            .unwrap();
        assert!(configured.data.error.is_none());

        let view: SymbolConfig =
            serde_json::from_str(&client.read(&"symbol_config".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(config, view);
        assert_eq!("QQQ", view.base_asset.code);
        assert_eq!(AssetClass::Etf, view.base_asset.class);
        assert_eq!("USD", view.quote_asset.code);
        assert_eq!(AssetClass::Fiat, view.quote_asset.class);

        // A maker sells one QQQ share at 400.10; both side fees are quote.
        let maker = Order::new(OrderSide::Sell, fixed("400.10"), fixed("1"), 0)
            .with_fee_rates(fixed("0.001"), fixed("0.002"));
        let rested = client
            .write(&MatchEngineRequest::place(maker))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(MatchEventKind::Rested, rested.data.events[0].kind);

        let taker = Order::new(OrderSide::Buy, fixed("400.10"), fixed("1000"), 0)
            .with_volume_unit(VolumeUnit::Quote)
            .with_fee_rates(fixed("0.003"), fixed("0.004"));
        let filled = client
            .write(&MatchEngineRequest::place(taker))
            .await
            .unwrap()
            .unwrap();
        let fill = &filled.data.events[0];
        assert_eq!(MatchEventKind::Fill, fill.kind);
        assert_eq!(fixed("1"), fill.volume);
        assert_eq!(fixed("400.1"), fill.quote_volume);
        assert_eq!(VolumeUnit::Base, fill.volume_unit);
        assert_eq!(fixed("1.6004"), fill.taker_fee);
        assert_eq!(Some(FeeAsset::Quote), fill.taker_fee_asset);
        assert_eq!(fixed("0.4001"), fill.maker_fee);
        assert_eq!(Some(FeeAsset::Quote), fill.maker_fee_asset);

        // The remaining quote budget rests as a normalized base bid and keeps
        // the original requested quote amount for audit.
        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!("1.4993", orders[0]["volume"]);
        assert_eq!("base", orders[0]["volume_unit"]);
        assert_eq!("1000", orders[0]["requested_volume"]);
        assert_eq!("quote", orders[0]["requested_volume_unit"]);

        // Tick/lot and asset precision are enforced through the API.
        let bad_price = client
            .write(&MatchEngineRequest::place(
                Order::new(OrderSide::Buy, fixed("400.101"), fixed("1"), 0)
                    .with_volume_unit(VolumeUnit::Quote),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            Some(RejectReason::InvalidOrder),
            bad_price.data.events[0].reason
        );

        let snapshot = node.state_machine.persist_snapshot().await.unwrap();
        assert!(snapshot.last_log_id.is_some());
        let bytes = std::fs::read(storage.snapshot_dir(1).join("current.snap")).unwrap();
        assert_eq!(
            11,
            u32::from_le_bytes(bytes[7..11].try_into().unwrap()),
            "snapshot must be v11"
        );

        node.shutdown().await.unwrap();
    });
}

#[test]
fn btc_usdt_quote_volume_and_received_fee_deduction() {
    C::run(async {
        let temp = TempDir::new("fee-assets-btc");
        let storage = StorageConfig::new(temp.path()).with_instance_prefix("fee-assets-btc");

        let node = build_match_engine_node(1, BTC_ADDR.to_string(), default_raft_config(), storage)
            .await
            .unwrap();
        let client = MatchEngineClient::<C>::new(1, BTC_ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        // Crypto/stablecoin defaults to received-asset fees: the buyer pays in
        // BTC, the seller in USDT.
        let config = SymbolConfig::new(0, 2, 8)
            .unwrap()
            .with_assets(
                AssetId::new("BTC", AssetClass::Crypto, 8).unwrap(),
                AssetId::new("USDT", AssetClass::Stablecoin, 2).unwrap(),
            )
            .unwrap()
            .with_tick_and_lot(fixed("0.01"), fixed("0.00000001"));
        assert_eq!(FeeAssetPolicy::Received, config.fee_asset_policy);
        client
            .write(&MatchEngineRequest::ConfigureSymbol { config })
            .await
            .unwrap()
            .unwrap();

        let view: SymbolConfig =
            serde_json::from_str(&client.read(&"symbol_config".to_string()).await.unwrap())
                .unwrap();
        assert_eq!("BTC", view.base_asset.code);
        assert_eq!(AssetClass::Crypto, view.base_asset.class);
        assert_eq!("USDT", view.quote_asset.code);
        assert_eq!(AssetClass::Stablecoin, view.quote_asset.class);

        // A maker sells 0.5 BTC at 60,000.00 USDT.
        let maker = Order::new(OrderSide::Sell, fixed("60000.00"), fixed("0.5"), 0)
            .with_fee_rates(fixed("0.002"), fixed("0"));
        let rested = client
            .write(&MatchEngineRequest::place(maker))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(MatchEventKind::Rested, rested.data.events[0].kind);

        // The taker submits 60,000 USDT of quote volume, normalized to 1 BTC.
        // It fills 0.5 BTC (30,000 notional) and rests the remaining 0.5 BTC.
        let taker = Order::new(OrderSide::Buy, fixed("60000.00"), fixed("60000"), 0)
            .with_volume_unit(VolumeUnit::Quote)
            .with_fee_rates(fixed("0"), fixed("0.001"));
        let filled = client
            .write(&MatchEngineRequest::place(taker))
            .await
            .unwrap()
            .unwrap();
        let fill = &filled.data.events[0];
        assert_eq!(MatchEventKind::Fill, fill.kind);
        assert_eq!(fixed("0.5"), fill.volume);
        assert_eq!(fixed("30000"), fill.quote_volume);
        // Taker fee: 0.5 BTC x 0.001 charged in the received base asset.
        assert_eq!(fixed("0.0005"), fill.taker_fee);
        assert_eq!(Some(FeeAsset::Base), fill.taker_fee_asset);
        // Maker fee: 30,000 x 0.002 charged in the received quote asset.
        assert_eq!(fixed("60"), fill.maker_fee);
        assert_eq!(Some(FeeAsset::Quote), fill.maker_fee_asset);

        let orders: serde_json::Value =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!("0.5", orders[0]["volume"]);
        assert_eq!("base", orders[0]["volume_unit"]);
        assert_eq!("60000", orders[0]["requested_volume"]);
        assert_eq!("quote", orders[0]["requested_volume_unit"]);

        // Price tick and base lot are enforced through the API.
        let bad_price = client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                fixed("60000.001"),
                fixed("1"),
                0,
            )))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            Some(RejectReason::InvalidOrder),
            bad_price.data.events[0].reason
        );

        let bad_lot = client
            .write(&MatchEngineRequest::place(Order::new(
                OrderSide::Buy,
                fixed("60000.00"),
                fixed("0.000000001"),
                0,
            )))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            Some(RejectReason::InvalidOrder),
            bad_lot.data.events[0].reason
        );

        node.shutdown().await.unwrap();
    });
}

async fn wait_for_leader(client: &MatchEngineClient<C>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(metrics) = client.metrics().await
            && metrics.current_leader == Some(1)
        {
            return;
        }
        assert!(Instant::now() <= deadline, "node 1 never became leader");
        C::sleep(Duration::from_millis(100)).await;
    }
}
