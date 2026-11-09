//! Write admission limit and `/admission/status` integration coverage.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::StorageConfig;
use matchengine_raft::build_match_engine_node;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::default_raft_config;
use matchengine_raft::network::admission::AdmissionStatus;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::store::MatchEngineRequest;
use openraft::type_config::TypeConfigExt;
use tokio::sync::Barrier;

use crate::common::TempDir;

type C = MatchEngineTypeConfig;

const ADDR: &str = "127.0.0.1:21933";
const CONCURRENT_WRITES: usize = 64;

#[test]
fn admission_limit_rejects_excess_writes_and_reports_counters() {
    C::run(async {
        let temp = TempDir::new("admission");
        let storage = StorageConfig::new(temp.path())
            .with_instance_prefix("admission")
            .with_max_inflight_writes(1);
        let node = build_match_engine_node(1, ADDR.to_string(), default_raft_config(), storage)
            .await
            .unwrap();

        let client = MatchEngineClient::<C>::new(1, ADDR.to_string());
        client.init().await.unwrap().unwrap();
        wait_for_leader(&client).await;

        let http = reqwest::Client::builder()
            .no_proxy()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap();
        let status: AdmissionStatus = http
            .get(format!("http://{ADDR}/admission/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(1, status.limit);
        assert_eq!(0, status.in_flight);

        let body = serde_json::to_vec(&MatchEngineRequest::place(Order::new(
            OrderSide::Buy,
            100.0,
            1.0,
            0,
        )))
        .unwrap();
        let barrier = Arc::new(Barrier::new(CONCURRENT_WRITES + 1));
        let mut handles = Vec::with_capacity(CONCURRENT_WRITES);
        for _ in 0..CONCURRENT_WRITES {
            let http = http.clone();
            let body = body.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                http.post(format!("http://{ADDR}/write"))
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }));
        }
        barrier.wait().await;

        let mut accepted = 0;
        let mut rejected = 0;
        for handle in handles {
            match handle.await.unwrap() {
                200 => accepted += 1,
                429 => rejected += 1,
                status => panic!("unexpected /write status {status}"),
            }
        }
        assert_eq!(CONCURRENT_WRITES, accepted + rejected);
        assert!(accepted > 0, "at least one write must be admitted");
        assert!(rejected > 0, "the one-slot limit must reject some writes");

        let status: AdmissionStatus = http
            .get(format!("http://{ADDR}/admission/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(1, status.limit);
        assert_eq!(0, status.in_flight);
        assert_eq!(accepted as u64, status.admitted);
        assert_eq!(rejected as u64, status.rejected);
        assert_eq!(1, status.peak);

        // The same gate covers /write_batch; one in-flight batch is enough to
        // reject the rest of the burst.
        let batch = serde_json::to_vec(&vec![
            MatchEngineRequest::place(Order::new(OrderSide::Buy, 200.0, 1.0, 0)),
            MatchEngineRequest::place(Order::new(OrderSide::Buy, 201.0, 1.0, 0)),
        ])
        .unwrap();
        let barrier = Arc::new(Barrier::new(CONCURRENT_WRITES + 1));
        let mut handles = Vec::with_capacity(CONCURRENT_WRITES);
        for _ in 0..CONCURRENT_WRITES {
            let http = http.clone();
            let batch = batch.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                http.post(format!("http://{ADDR}/write_batch"))
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(batch)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }));
        }
        barrier.wait().await;

        let mut accepted_batches = 0;
        let mut rejected_batches = 0;
        for handle in handles {
            match handle.await.unwrap() {
                200 => accepted_batches += 1,
                429 => rejected_batches += 1,
                status => panic!("unexpected /write_batch status {status}"),
            }
        }
        assert_eq!(CONCURRENT_WRITES, accepted_batches + rejected_batches);
        assert!(accepted_batches > 0, "at least one batch must be admitted");
        assert!(
            rejected_batches > 0,
            "the one-slot limit must reject some batches"
        );

        let status: AdmissionStatus = http
            .get(format!("http://{ADDR}/admission/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(1, status.limit);
        assert_eq!(0, status.in_flight);
        assert_eq!(accepted as u64 + accepted_batches as u64, status.admitted);
        assert_eq!(rejected as u64 + rejected_batches as u64, status.rejected);
        assert_eq!(1, status.peak);

        // Every admitted request was applied: one order per admitted /write,
        // two per admitted /write_batch.
        let expected_orders = accepted + accepted_batches * 2;
        assert_eq!(
            expected_orders.to_string(),
            client
                .read(&"orderbook_sequence".to_string())
                .await
                .unwrap()
        );
        let orders: Vec<Order> =
            serde_json::from_str(&client.read(&"orderbook_orders".to_string()).await.unwrap())
                .unwrap();
        assert_eq!(expected_orders, orders.len());

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
