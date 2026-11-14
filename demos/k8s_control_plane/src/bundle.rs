//! A bundle is the unit the CLI renders and the manager seeds: one cell, one shard, and the
//! symbols/deployments/routes bound to that shard. It mirrors a directory of CRs without
//! requiring a YAML parser.

use serde::{Deserialize, Serialize};

use crate::model::{
    AssetClass, AssetId, Cell, CellCapacity, CellSpec, DeploymentSpec, DeploymentState,
    FeeAssetPolicy, GatewayRef, Observability, Placement, ShardClass, ShardSpec, StpMode,
    SymbolDeployment, SymbolRoute, SymbolShard, TradingSymbol, TradingSymbolSpec,
};

/// A renderable, self-contained slice of the control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardBundle {
    pub cell: Cell,
    pub shard: SymbolShard,
    #[serde(default)]
    pub symbols: Vec<TradingSymbol>,
    #[serde(default)]
    pub deployments: Vec<SymbolDeployment>,
    #[serde(default)]
    pub routes: Vec<SymbolRoute>,
}

impl ShardBundle {
    /// Every deployment must resolve to a symbol in the bundle.
    pub fn deployment_symbols(&self) -> Vec<(&SymbolDeployment, Option<&TradingSymbol>)> {
        self.deployments
            .iter()
            .map(|d| (d, self.symbols.iter().find(|s| s.name == d.spec.symbol_ref)))
            .collect()
    }
}

fn asset(code: &str, class: AssetClass, decimals: u8) -> AssetId {
    AssetId {
        code: code.to_string(),
        class,
        decimals,
    }
}

struct RuleSet<'a> {
    price_decimals: u8,
    price_tick: &'a str,
    base_lot: &'a str,
    fee_asset_policy: FeeAssetPolicy,
    stp: StpMode,
}

fn symbol(
    name: &str,
    namespace: &str,
    symbol_id: u32,
    base: AssetId,
    quote: AssetId,
    rules: RuleSet<'_>,
    owner: &str,
) -> TradingSymbol {
    TradingSymbol {
        name: name.to_string(),
        namespace: namespace.to_string(),
        spec: TradingSymbolSpec {
            symbol_id,
            base,
            quote,
            price_decimals: rules.price_decimals,
            price_tick: rules.price_tick.to_string(),
            base_lot: rules.base_lot.to_string(),
            fee_asset_policy: rules.fee_asset_policy,
            stp: rules.stp,
            owner: owner.to_string(),
            generation: 1,
        },
        status: Default::default(),
    }
}

fn deployment(
    name: &str,
    namespace: &str,
    symbol_ref: &str,
    shard_ref: &str,
    slot: u16,
    http_port: u16,
) -> SymbolDeployment {
    SymbolDeployment {
        name: name.to_string(),
        namespace: namespace.to_string(),
        spec: DeploymentSpec {
            symbol_ref: symbol_ref.to_string(),
            generation: 1,
            shard_ref: shard_ref.to_string(),
            placement: Placement { slot, http_port },
            image: None,
            resources: None,
            config_hash: String::new(),
            desired_state: DeploymentState::Active,
        },
        status: Default::default(),
    }
}

fn route(
    name: &str,
    namespace: &str,
    symbol_ref: &str,
    cell_ref: &str,
    deployment_ref: &str,
) -> SymbolRoute {
    SymbolRoute {
        name: name.to_string(),
        namespace: namespace.to_string(),
        spec: crate::model::RouteSpec {
            symbol_ref: symbol_ref.to_string(),
            cell_ref: cell_ref.to_string(),
            active_deployment_ref: deployment_ref.to_string(),
            previous_deployment_ref: None,
            state: Default::default(),
        },
        status: Default::default(),
    }
}

/// Demo catalog: one packed FX shard with two symbols, one reserved empty packed shard, and
/// one dedicated crypto shard.
///
/// Ports follow the documented convention (`21001 + slot`, Raft `+1000`). The images are
/// placeholders; replace them with a signed digest for a real cluster.
pub fn demo_bundles() -> Vec<ShardBundle> {
    let cell = Cell {
        name: "eu-1".to_string(),
        namespace: "match-eu-1".to_string(),
        spec: CellSpec {
            cluster_id: "eu-1-prod".to_string(),
            region: "eu-west".to_string(),
            kube_context: "orbstack".to_string(),
            namespaces: vec!["match-eu-1".to_string()],
            storage_classes: vec!["local-path".to_string(), "fast-ssd".to_string()],
            capacity: CellCapacity {
                max_shards: 120,
                max_processes: 9600,
            },
            gateway: GatewayRef {
                url: "https://match-eu-1.example.com".to_string(),
            },
            observability: Observability {
                prometheus: "https://prometheus-eu-1.example.com".to_string(),
            },
        },
    };

    let packed = ShardBundle {
        cell: cell.clone(),
        shard: SymbolShard {
            name: "fx-eu-01".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: ShardSpec {
                cell: "eu-1".to_string(),
                environment: "prod".to_string(),
                class: ShardClass::Packed,
                replicas: 4,
                packing: Default::default(),
                image: crate::DEFAULT_IMAGE.to_string(),
                release: Default::default(),
                storage: crate::model::StorageSpec {
                    class_name: "local-path".to_string(),
                    size: "20Gi".to_string(),
                },
                default_process_resources: crate::model::Resources::new(
                    "250m", "512Mi", "1", "1Gi",
                ),
                raft: crate::model::RaftSettings {
                    wal_fsync: crate::model::WalFsync::every(),
                    ..Default::default()
                },
                ..Default::default()
            },
            status: Default::default(),
        },
        symbols: vec![
            symbol(
                "eurusd",
                "match-eu-1",
                1,
                asset("EUR", AssetClass::Fiat, 2),
                asset("USD", AssetClass::Fiat, 2),
                RuleSet {
                    price_decimals: 5,
                    price_tick: "0.00001",
                    base_lot: "0.01",
                    fee_asset_policy: FeeAssetPolicy::Received,
                    stp: StpMode::Off,
                },
                "fx-team",
            ),
            symbol(
                "gbpusd",
                "match-eu-1",
                2,
                asset("GBP", AssetClass::Fiat, 2),
                asset("USD", AssetClass::Fiat, 2),
                RuleSet {
                    price_decimals: 5,
                    price_tick: "0.00001",
                    base_lot: "0.01",
                    fee_asset_policy: FeeAssetPolicy::Received,
                    stp: StpMode::Off,
                },
                "fx-team",
            ),
        ],
        deployments: vec![
            deployment("eurusd-g1", "match-eu-1", "eurusd", "fx-eu-01", 0, 21001),
            deployment("gbpusd-g1", "match-eu-1", "gbpusd", "fx-eu-01", 1, 21002),
        ],
        routes: vec![
            route("eurusd", "match-eu-1", "eurusd", "eu-1", "eurusd-g1"),
            route("gbpusd", "match-eu-1", "gbpusd", "eu-1", "gbpusd-g1"),
        ],
    };

    // A reserved packed shard: capacity and a port range are allocated, but no engine
    // process exists until the first SymbolDeployment is bound.
    let reserved = ShardBundle {
        cell: cell.clone(),
        shard: SymbolShard {
            name: "fx-eu-02".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: ShardSpec {
                cell: "eu-1".to_string(),
                environment: "prod".to_string(),
                class: ShardClass::Packed,
                replicas: 4,
                packing: crate::model::Packing {
                    mode: crate::model::PackingMode::Containers,
                    max_processes_per_pod: 4,
                },
                image: crate::DEFAULT_IMAGE.to_string(),
                release: Default::default(),
                storage: crate::model::StorageSpec {
                    class_name: "local-path".to_string(),
                    size: "2Gi".to_string(),
                },
                default_process_resources: crate::model::Resources::new(
                    "250m", "512Mi", "1", "1Gi",
                ),
                raft: crate::model::RaftSettings {
                    wal_fsync: crate::model::WalFsync::every(),
                    ..Default::default()
                },
                network: crate::model::NetworkSettings {
                    http_base_port: 21041,
                },
                ..Default::default()
            },
            status: crate::model::ShardStatus {
                phase: crate::model::Phase::Reserved,
                ..Default::default()
            },
        },
        symbols: Vec::new(),
        deployments: Vec::new(),
        routes: Vec::new(),
    };

    let dedicated = ShardBundle {
        cell,
        shard: SymbolShard {
            name: "me-btcusdt".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: ShardSpec {
                cell: "eu-1".to_string(),
                environment: "prod".to_string(),
                class: ShardClass::Dedicated,
                replicas: 4,
                packing: crate::model::Packing {
                    mode: crate::model::PackingMode::Containers,
                    max_processes_per_pod: 1,
                },
                image: crate::DEFAULT_IMAGE.to_string(),
                release: Default::default(),
                storage: crate::model::StorageSpec {
                    class_name: "local-path".to_string(),
                    size: "20Gi".to_string(),
                },
                default_process_resources: crate::model::Resources::new("1", "2Gi", "1", "2Gi"),
                raft: crate::model::RaftSettings {
                    wal_fsync: crate::model::WalFsync::every(),
                    ..Default::default()
                },
                ..Default::default()
            },
            status: Default::default(),
        },
        symbols: vec![symbol(
            "btcusdt",
            "match-eu-1",
            3,
            asset("BTC", AssetClass::Crypto, 8),
            asset("USDT", AssetClass::Stablecoin, 6),
            RuleSet {
                price_decimals: 2,
                price_tick: "0.01",
                base_lot: "0.00001",
                fee_asset_policy: FeeAssetPolicy::Received,
                stp: StpMode::CancelResting,
            },
            "crypto-team",
        )],
        deployments: vec![deployment(
            "btcusdt-g1",
            "match-eu-1",
            "btcusdt",
            "me-btcusdt",
            0,
            21001,
        )],
        routes: vec![route(
            "btcusdt",
            "match-eu-1",
            "btcusdt",
            "eu-1",
            "btcusdt-g1",
        )],
    };

    vec![packed, reserved, dedicated]
}
