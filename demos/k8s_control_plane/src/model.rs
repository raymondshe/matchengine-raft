//! CRD-shaped data model.
//!
//! Field names follow the JSON/YAML contract in `docs/KUBERNETES_CONTROL_PLANE.md` §6
//! (camelCase). The manager is the writer of desired state; the operator writes status.
//! Everything here is plain data so it can be persisted as JSON, validated, and rendered.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Shared enums and small types
// ---------------------------------------------------------------------------

/// Asset class carried by a symbol's base/quote metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssetClass {
    Fiat,
    Stablecoin,
    Crypto,
    Equity,
    Etf,
    #[default]
    Unknown,
}

impl AssetClass {
    /// Brokerage-style assets are billed in the quote currency.
    pub fn is_equity_like(self) -> bool {
        matches!(self, Self::Equity | Self::Etf)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fiat => "fiat",
            Self::Stablecoin => "stablecoin",
            Self::Crypto => "crypto",
            Self::Equity => "equity",
            Self::Etf => "etf",
            Self::Unknown => "unknown",
        }
    }
}

/// Which asset a fill fee is charged in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FeeAssetPolicy {
    /// Buyer pays in the received base, seller in the received quote.
    #[default]
    Received,
    /// Both sides pay in the quote asset.
    Quote,
    /// Both sides pay in the base asset.
    Base,
}

impl FeeAssetPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Quote => "quote",
            Self::Base => "base",
        }
    }
}

/// Self-trade prevention mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StpMode {
    #[default]
    Off,
    CancelResting,
    CancelAggressor,
    CancelBoth,
}

impl StpMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::CancelResting => "cancel_resting",
            Self::CancelAggressor => "cancel_aggressor",
            Self::CancelBoth => "cancel_both",
        }
    }
}

/// Base or quote asset identity and precision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetId {
    pub code: String,
    pub class: AssetClass,
    pub decimals: u8,
}

/// Node-local status phase. Kept deliberately small; conditions carry the detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum Phase {
    #[default]
    Pending,
    Bootstrapped,
    Ready,
    Degraded,
    Failed,
    Running,
    Succeeded,
    Cancelled,
    /// An empty shard reservation: capacity is allocated, but no SymbolDeployment is bound.
    Reserved,
    /// The deployment has been shut down; its data and slot are retained.
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Condition {
    pub fn true_(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            status: "True".to_string(),
            reason: None,
            message: None,
        }
    }

    pub fn false_(kind: &str, reason: &str) -> Self {
        Self {
            kind: kind.to_string(),
            status: "False".to_string(),
            reason: Some(reason.to_string()),
            message: None,
        }
    }
}

/// Container/pod resource quantities as Kubernetes strings (`250m`, `512Mi`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResourceList {
    #[serde(default)]
    pub cpu: String,
    #[serde(default)]
    pub memory: String,
}

impl ResourceList {
    pub fn new(cpu: &str, memory: &str) -> Self {
        Self {
            cpu: cpu.to_string(),
            memory: memory.to_string(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.cpu.is_empty() && self.memory.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    #[serde(default)]
    pub requests: ResourceList,
    #[serde(default)]
    pub limits: ResourceList,
}

impl Resources {
    pub fn new(cpu_request: &str, mem_request: &str, cpu_limit: &str, mem_limit: &str) -> Self {
        Self {
            requests: ResourceList::new(cpu_request, mem_request),
            limits: ResourceList::new(cpu_limit, mem_limit),
        }
    }
}

// ---------------------------------------------------------------------------
// TradingSymbol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradingSymbol {
    pub name: String,
    #[serde(default)]
    pub namespace: String,
    pub spec: TradingSymbolSpec,
    #[serde(default)]
    pub status: TradingSymbolStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradingSymbolSpec {
    pub symbol_id: u32,
    pub base: AssetId,
    pub quote: AssetId,
    pub price_decimals: u8,
    /// Minimum price increment, e.g. `"0.00001"`.
    pub price_tick: String,
    /// Minimum base quantity increment, e.g. `"0.01"`.
    pub base_lot: String,
    #[serde(default)]
    pub fee_asset_policy: FeeAssetPolicy,
    #[serde(default)]
    pub stp: StpMode,
    #[serde(default)]
    pub owner: String,
    #[serde(default = "default_generation")]
    pub generation: u32,
}

fn default_generation() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TradingSymbolStatus {
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub config_locked: bool,
    #[serde(default)]
    pub config_hash: String,
    #[serde(default)]
    pub applied_by: String,
}

// ---------------------------------------------------------------------------
// SymbolShard
// ---------------------------------------------------------------------------

/// A shard is one StatefulSet with four (or more) pods. A dedicated shard hosts one symbol;
/// a packed shard hosts many. The shard declares capacity and pod template inputs only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolShard {
    pub name: String,
    #[serde(default)]
    pub namespace: String,
    pub spec: ShardSpec,
    #[serde(default)]
    pub status: ShardStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardSpec {
    pub cell: String,
    #[serde(default = "default_environment")]
    pub environment: String,
    #[serde(default)]
    pub class: ShardClass,
    #[serde(default = "default_replicas")]
    pub replicas: u8,
    #[serde(default)]
    pub packing: Packing,
    pub image: String,
    #[serde(default)]
    pub release: Release,
    #[serde(default)]
    pub storage: StorageSpec,
    #[serde(default)]
    pub default_process_resources: Resources,
    #[serde(default)]
    pub raft: RaftSettings,
    #[serde(default)]
    pub quotas: Quotas,
    #[serde(default)]
    pub network: NetworkSettings,
    #[serde(default)]
    pub upgrade: UpgradeSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupSchedule>,
    #[serde(default)]
    pub paused: bool,
}

fn default_environment() -> String {
    "prod".to_string()
}

fn default_replicas() -> u8 {
    4
}

impl Default for ShardSpec {
    fn default() -> Self {
        Self {
            cell: String::new(),
            environment: default_environment(),
            class: ShardClass::default(),
            replicas: default_replicas(),
            packing: Packing::default(),
            image: crate::DEFAULT_IMAGE.to_string(),
            release: Release::default(),
            storage: StorageSpec::default(),
            default_process_resources: Resources::default(),
            raft: RaftSettings::default(),
            quotas: Quotas::default(),
            network: NetworkSettings::default(),
            upgrade: UpgradeSettings::default(),
            backup: None,
            paused: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum ShardClass {
    Dedicated,
    #[default]
    Packed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PackingMode {
    #[default]
    Containers,
    Supervisor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Packing {
    #[serde(default)]
    pub mode: PackingMode,
    #[serde(default = "default_max_processes")]
    pub max_processes_per_pod: u16,
}

fn default_max_processes() -> u16 {
    20
}

impl Default for Packing {
    fn default() -> Self {
        Self {
            mode: PackingMode::Containers,
            max_processes_per_pod: default_max_processes(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub wire_version: u16,
    pub snapshot_version: u16,
    /// Reserved for a future release that can tolerate mixed-version peers. Same-version
    /// rolling is always allowed and does not depend on this flag.
    #[serde(default)]
    pub mixed_version_safe: bool,
}

impl Default for Release {
    fn default() -> Self {
        Self {
            wire_version: 7,
            snapshot_version: 11,
            mixed_version_safe: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    #[serde(default = "default_storage_class")]
    pub class_name: String,
    #[serde(default = "default_storage_size")]
    pub size: String,
}

fn default_storage_class() -> String {
    "fast-ssd".to_string()
}

fn default_storage_size() -> String {
    "50Gi".to_string()
}

impl Default for StorageSpec {
    fn default() -> Self {
        Self {
            class_name: default_storage_class(),
            size: default_storage_size(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RaftSettings {
    #[serde(default)]
    pub wal_fsync: WalFsync,
    #[serde(default)]
    pub read_policy: ReadPolicy,
    #[serde(default)]
    pub snapshot: SnapshotSettings,
}

/// WAL fsync policy. `none` means the env var is left unset (fastest, but the binary then
/// requires four voters); `every` maps to `RAFT_WAL_FSYNC_MS=0`; `interval` maps to `N`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalFsync {
    #[serde(default)]
    pub mode: FsyncMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FsyncMode {
    #[default]
    Every,
    None,
    Interval,
}

impl Default for WalFsync {
    fn default() -> Self {
        Self {
            mode: FsyncMode::Every,
            interval_ms: None,
        }
    }
}

impl WalFsync {
    pub fn every() -> Self {
        Self::default()
    }

    pub fn none() -> Self {
        Self {
            mode: FsyncMode::None,
            interval_ms: None,
        }
    }

    pub fn interval(ms: u64) -> Self {
        Self {
            mode: FsyncMode::Interval,
            interval_ms: Some(ms),
        }
    }

    /// Value for `RAFT_WAL_FSYNC_MS`, or `None` to leave the variable unset.
    pub fn env_value(&self) -> Option<String> {
        match self.mode {
            FsyncMode::Every => Some("0".to_string()),
            FsyncMode::None => None,
            FsyncMode::Interval => Some(self.interval_ms.unwrap_or(100).to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReadPolicy {
    #[default]
    LocalLease,
    Lease,
    ReadIndex,
}

impl ReadPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalLease => "local_lease",
            Self::Lease => "lease",
            Self::ReadIndex => "read_index",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSettings {
    /// Maps to `--snapshot-interval-ms`. The log-count policy is fixed at 100_000 today.
    #[serde(default = "default_snapshot_minutes")]
    pub every_minutes: u32,
}

fn default_snapshot_minutes() -> u32 {
    30
}

impl Default for SnapshotSettings {
    fn default() -> Self {
        Self {
            every_minutes: default_snapshot_minutes(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Quotas {
    #[serde(default)]
    pub per_process: ProcessQuota,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessQuota {
    #[serde(default = "default_max_events")]
    pub max_events: u64,
    #[serde(default = "default_max_inflight")]
    pub max_inflight_writes: u32,
}

fn default_max_events() -> u64 {
    262_144
}

fn default_max_inflight() -> u32 {
    1024
}

impl Default for ProcessQuota {
    fn default() -> Self {
        Self {
            max_events: default_max_events(),
            max_inflight_writes: default_max_inflight(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSettings {
    #[serde(default = "default_http_base_port")]
    pub http_base_port: u16,
}

fn default_http_base_port() -> u16 {
    21001
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            http_base_port: default_http_base_port(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeStrategy {
    #[default]
    Auto,
    Rolling,
    StopTheWorld,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeSettings {
    #[serde(default)]
    pub strategy: UpgradeStrategy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_window: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSchedule {
    pub schedule: String,
    pub destination: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ShardStatus {
    #[serde(default)]
    pub observed_generation: u64,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub members_per_process: u8,
    #[serde(default)]
    pub capacity: ShardCapacity,
    #[serde(default)]
    pub symbols: Vec<ShardSymbolStatus>,
    #[serde(default)]
    pub nodes: Vec<ShardNodeStatus>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ShardCapacity {
    pub processes: u16,
    pub cpu_request: String,
    pub memory_request: String,
    pub storage_request: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardSymbolStatus {
    pub name: String,
    pub deployment_ref: String,
    pub generation: u32,
    pub slot: u16,
    pub http_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_pod: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_addr: Option<String>,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag: Option<u64>,
    #[serde(default)]
    pub config_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardNodeStatus {
    pub name: String,
    pub id: u8,
    pub ready: bool,
    pub processes: u16,
}

// ---------------------------------------------------------------------------
// SymbolDeployment and SymbolRoute
// ---------------------------------------------------------------------------

/// One symbol generation's placement. This object is the single owner of slot/HTTP port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDeployment {
    pub name: String,
    #[serde(default)]
    pub namespace: String,
    pub spec: DeploymentSpec,
    #[serde(default)]
    pub status: DeploymentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentSpec {
    pub symbol_ref: String,
    #[serde(default = "default_generation")]
    pub generation: u32,
    pub shard_ref: String,
    pub placement: Placement,
    /// Optional per-generation image override. Empty inherits `SymbolShard.spec.image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Optional per-process resources override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(default)]
    pub config_hash: String,
    #[serde(default)]
    pub desired_state: DeploymentState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Placement {
    pub slot: u16,
    pub http_port: u16,
}

impl Placement {
    /// Raft is always the HTTP port plus the fixed engine offset.
    pub fn raft_port(&self) -> u16 {
        self.http_port.saturating_add(RAFT_PORT_OFFSET)
    }
}

/// The engine hardcodes `raft = http + 1000` (`RAFT_PORT_OFFSET`).
pub const RAFT_PORT_OFFSET: u16 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum DeploymentState {
    #[default]
    Active,
    Shadow,
    Draining,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStatus {
    #[serde(default)]
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_pod: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_addr: Option<String>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrapped_at: Option<String>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// Gateway routing intent: which deployment generation receives traffic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolRoute {
    pub name: String,
    #[serde(default)]
    pub namespace: String,
    pub spec: RouteSpec,
    #[serde(default)]
    pub status: RouteStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteSpec {
    pub symbol_ref: String,
    pub cell_ref: String,
    pub active_deployment_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_deployment_ref: Option<String>,
    #[serde(default)]
    pub state: RouteState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum RouteState {
    #[default]
    Active,
    Shadow,
    Cutover,
    Draining,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RouteStatus {
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub observed_generation: u64,
    #[serde(default)]
    pub active_deployment_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cutover_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Cell
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cell {
    pub name: String,
    #[serde(default)]
    pub namespace: String,
    pub spec: CellSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellSpec {
    /// Unique; stamped on every object, metric and log line.
    pub cluster_id: String,
    pub region: String,
    pub kube_context: String,
    #[serde(default)]
    pub namespaces: Vec<String>,
    #[serde(default)]
    pub storage_classes: Vec<String>,
    #[serde(default)]
    pub capacity: CellCapacity,
    #[serde(default)]
    pub gateway: GatewayRef,
    #[serde(default)]
    pub observability: Observability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellCapacity {
    pub max_shards: u32,
    pub max_processes: u32,
}

impl Default for CellCapacity {
    fn default() -> Self {
        Self {
            max_shards: 120,
            max_processes: 9600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRef {
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Observability {
    #[serde(default)]
    pub prometheus: String,
}

// ---------------------------------------------------------------------------
// Operation, Backup, ClusterTemplate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum OperationKind {
    AddSymbol,
    RemoveSymbol,
    MigrateSymbol,
    Scale,
    Upgrade,
    Snapshot,
    Backup,
    TransferLeader,
    Pause,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub name: String,
    pub spec: OperationSpec,
    #[serde(default)]
    pub status: OperationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationSpec {
    pub shard_ref: String,
    pub kind: OperationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_image: Option<String>,
    #[serde(default)]
    pub strategy: UpgradeStrategy,
    #[serde(default)]
    pub idempotency_key: String,
    #[serde(default)]
    pub requested_by: String,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OperationStatus {
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub completed_steps: u16,
    #[serde(default)]
    pub total_steps: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Backup {
    pub name: String,
    pub spec: BackupSpec,
    #[serde(default)]
    pub status: BackupStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSpec {
    pub shard_ref: String,
    #[serde(default)]
    pub mode: BackupMode,
    #[serde(default)]
    pub volume_group_snapshot_class: String,
    pub destination: String,
    #[serde(default)]
    pub quiesce: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BackupMode {
    #[default]
    VolumeGroup,
    Logical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackupStatus {
    #[serde(default)]
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_group_snapshot: Option<String>,
    #[serde(default)]
    pub processes: Vec<BackupProcessStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupProcessStatus {
    pub name: String,
    #[serde(default)]
    pub snapshot_index: u64,
    #[serde(default)]
    pub last_log_index: u64,
    #[serde(default)]
    pub last_purged: u64,
    #[serde(default)]
    pub log_state_sha256: String,
    #[serde(default)]
    pub snapshot_sha256: String,
    #[serde(default)]
    pub wal_bytes: u64,
    #[serde(default)]
    pub s3_prefix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_tested_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterTemplate {
    pub name: String,
    pub spec: TemplateSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateSpec {
    #[serde(default = "default_replicas")]
    pub replicas: u8,
    #[serde(default)]
    pub packing: Packing,
    pub image: String,
    #[serde(default)]
    pub storage: StorageSpec,
    #[serde(default)]
    pub default_process_resources: Resources,
    #[serde(default)]
    pub raft: RaftSettings,
    #[serde(default)]
    pub network: NetworkSettings,
}

impl Default for TemplateSpec {
    fn default() -> Self {
        Self {
            replicas: default_replicas(),
            packing: Packing::default(),
            image: crate::DEFAULT_IMAGE.to_string(),
            storage: StorageSpec::default(),
            default_process_resources: Resources::default(),
            raft: RaftSettings::default(),
            network: NetworkSettings::default(),
        }
    }
}

/// Built-in template used by the GUI when no template CR is present.
pub fn default_template() -> ClusterTemplate {
    ClusterTemplate {
        name: "prod-fx-packed".to_string(),
        spec: TemplateSpec::default(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Sanitize a string into a DNS-1123 label (max 63 chars).
pub fn dns_label(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for ch in input.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.truncate(63);
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() { "x".to_string() } else { out }
}

/// Kubernetes port names are capped at 15 characters, lowercase alphanumeric or `-`.
pub fn port_name(prefix: &str, slot: u16) -> String {
    let name = format!("{prefix}-{slot}");
    name.chars().take(15).collect()
}

/// Stable fingerprint of the symbol configuration the operator applies through
/// `ConfigureSymbol`. It is a non-cryptographic FNV-1a hash of the canonical JSON so the
/// manager, the operator and `/read symbol_config` comparisons in tests agree without pulling
/// in a hashing dependency.
pub fn config_fingerprint(spec: &TradingSymbolSpec) -> String {
    let bytes = serde_json::to_vec(spec).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Desired configuration hash for a deployment: an explicit `spec.configHash` wins, otherwise
/// the fingerprint of the bound symbol spec.
pub fn desired_config_hash(deployment: &SymbolDeployment, symbol: &TradingSymbol) -> String {
    if deployment.spec.config_hash.is_empty() {
        config_fingerprint(&symbol.spec)
    } else {
        deployment.spec.config_hash.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_labels_are_safe() {
        assert_eq!(dns_label("EUR/USD"), "eur-usd");
        assert_eq!(dns_label("--weird--"), "weird");
        assert_eq!(dns_label(""), "x");
        assert_eq!(dns_label("a".repeat(100).as_str()).len(), 63);
    }

    #[test]
    fn raft_port_is_http_plus_1000() {
        let placement = Placement {
            slot: 0,
            http_port: 21001,
        };
        assert_eq!(placement.raft_port(), 22001);
        let placement = Placement {
            slot: 19,
            http_port: 21020,
        };
        assert_eq!(placement.raft_port(), 22020);
    }

    #[test]
    fn wal_fsync_maps_to_env_values() {
        assert_eq!(WalFsync::every().env_value().as_deref(), Some("0"));
        assert_eq!(WalFsync::none().env_value(), None);
        assert_eq!(WalFsync::interval(100).env_value().as_deref(), Some("100"));
    }

    #[test]
    fn port_names_fit_kubernetes_limit() {
        assert_eq!(port_name("http", 0), "http-0");
        assert_eq!(port_name("raft", 19), "raft-19");
        assert!(port_name("very-long-prefix", 3).len() <= 15);
    }
}
