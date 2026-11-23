//! Preflight and placement validation.
//!
//! The rules encode the constraints that are expensive to discover at runtime: port/slot
//! uniqueness, the fixed `raft = http + 1000` offset, the four-voter requirement when WAL fsync
//! is unset, asset/fee-policy compatibility and resource sanity. Rendering refuses to emit a
//! manifest set when validation produces errors.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::Serialize;

use crate::bundle::ShardBundle;
use crate::model::{
    AssetClass, Cell, FeeAssetPolicy, FsyncMode, MarketDataTransportMode, PackingMode, ShardClass,
    SymbolDeployment, SymbolRoute, SymbolShard, TradingSymbol, WalFsync, aeron_channel_endpoint,
    aeron_channel_param, dns_label, is_multicast_ipv4,
};
use crate::store::Catalog;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub severity: Severity,
    pub code: String,
    pub object: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn merge(&mut self, other: Report) {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }

    pub fn error(&mut self, code: &str, object: &str, message: impl Into<String>) {
        self.errors.push(Issue {
            severity: Severity::Error,
            code: code.to_string(),
            object: object.to_string(),
            message: message.into(),
        });
    }

    pub fn warn(&mut self, code: &str, object: &str, message: impl Into<String>) {
        self.warnings.push(Issue {
            severity: Severity::Warning,
            code: code.to_string(),
            object: object.to_string(),
            message: message.into(),
        });
    }

    /// Keep only issues whose `object` is in `scope`.
    ///
    /// The catalog report is the source of truth; the manager endpoint uses this to show a
    /// shard-scoped view without hiding cross-shard errors from `/api/v1/validate`.
    pub fn scoped(&self, scope: &BTreeSet<String>) -> Report {
        let keep = |issue: &Issue| scope.contains(&issue.object);
        Report {
            errors: self.errors.iter().filter(|i| keep(i)).cloned().collect(),
            warnings: self.warnings.iter().filter(|i| keep(i)).cloned().collect(),
        }
    }

    pub fn require_ok(self) -> Result<(), Report> {
        if self.is_ok() { Ok(()) } else { Err(self) }
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for issue in self.errors.iter().chain(self.warnings.iter()) {
            let _ = writeln!(
                out,
                "{:?} [{}] {}: {}",
                issue.severity, issue.code, issue.object, issue.message
            );
        }
        out
    }
}

/// Aggregate resource request of one pod (all engine processes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PodTotals {
    pub processes: usize,
    pub cpu_request_millis: u64,
    pub memory_request_bytes: u64,
    pub storage_request_bytes: u64,
}

/// Validate one bundle (cell + shard + its symbols/deployments/routes).
pub fn validate_bundle(bundle: &ShardBundle) -> Report {
    let symbols: BTreeMap<&str, &TradingSymbol> = bundle
        .symbols
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect();
    let deployments: Vec<&SymbolDeployment> = bundle.deployments.iter().collect();
    let routes: Vec<&SymbolRoute> = bundle.routes.iter().collect();
    let mut report = Report::default();
    validate_cell(&bundle.cell, &[&bundle.shard], &mut report);
    for symbol in &bundle.symbols {
        report.merge(validate_symbol(symbol));
    }
    validate_shard(&bundle.shard, &symbols, &deployments, &routes, &mut report);
    report
}

/// Validate the whole catalog: all cells, shards, symbols, deployments and routes.
pub fn validate_catalog(catalog: &Catalog) -> Report {
    let mut report = Report::default();

    let shards: Vec<&SymbolShard> = catalog.shards.values().collect();
    for cell in catalog.cells.values() {
        let cell_shards: Vec<&SymbolShard> = shards
            .iter()
            .copied()
            .filter(|s| s.spec.cell == cell.name)
            .collect();
        validate_cell(cell, &cell_shards, &mut report);
    }

    let symbols: BTreeMap<&str, &TradingSymbol> = catalog
        .symbols
        .values()
        .map(|s| (s.name.as_str(), s))
        .collect();
    for symbol in catalog.symbols.values() {
        report.merge(validate_symbol(symbol));
    }
    for shard in catalog.shards.values() {
        let deployments = catalog.shard_deployments(&shard.name);
        let shard_symbols: BTreeMap<&str, &TradingSymbol> = deployments
            .iter()
            .filter_map(|d| {
                symbols
                    .get(d.spec.symbol_ref.as_str())
                    .map(|s| (s.name.as_str(), *s))
            })
            .collect();
        let routes: Vec<&SymbolRoute> = catalog
            .routes
            .values()
            .filter(|r| {
                catalog
                    .deployment(&r.spec.active_deployment_ref)
                    .is_some_and(|d| d.spec.shard_ref == shard.name)
            })
            .collect();
        validate_shard(shard, &shard_symbols, &deployments, &routes, &mut report);
    }
    validate_symbol_ids(catalog, &mut report);
    validate_market_data_services(catalog, &mut report);
    validate_order_systems(catalog, &mut report);
    report
}

fn validate_market_data_services(catalog: &Catalog, report: &mut Report) {
    let mut ws_ports: BTreeMap<(&str, u16), &str> = BTreeMap::new();
    for service in catalog.market_data_services.values() {
        let object = &service.name;
        if service.name.trim().is_empty() {
            report.error("market_data_name", object, "name must not be empty");
        } else if dns_label(&service.name) != service.name || service.name.len() > 63 {
            report.error(
                "market_data_name",
                object,
                "name must be a lowercase DNS label of at most 63 characters (it is also a label value)",
            );
        }
        if !catalog.cells.contains_key(&service.spec.cell) {
            report.error(
                "market_data_cell",
                object,
                format!("cell {} is not in the catalog", service.spec.cell),
            );
        }
        if service.spec.image.trim().is_empty() {
            report.error("market_data_image", object, "spec.image is required");
        } else if !service.spec.image.contains("@sha256:") {
            report.warn(
                "market_data_image_tag",
                object,
                "pin the market-data image by digest in production; a mutable tag cannot gate upgrades",
            );
        }
        if service.spec.replicas == 0 {
            report.error(
                "market_data_replicas",
                object,
                "replicas must be greater than zero",
            );
        }
        if service.spec.autoscaling.min_replicas == 0 {
            report.error(
                "market_data_min_replicas",
                object,
                "autoscaling.minReplicas must be at least 1",
            );
        }
        if service.spec.autoscaling.max_replicas != 0
            && service.spec.autoscaling.max_replicas < service.spec.min_replicas()
        {
            report.error(
                "market_data_max_replicas",
                object,
                "autoscaling.maxReplicas must be greater than or equal to minReplicas",
            );
        }
        if !(1..=100).contains(&service.spec.autoscaling.target_cpu_utilization) {
            report.error(
                "market_data_target_cpu",
                object,
                "autoscaling.targetCpuUtilization must be between 1 and 100",
            );
        }
        let cpu_request = service
            .spec
            .resources
            .as_ref()
            .map(|resources| resources.requests.cpu.as_str())
            .unwrap_or("100m");
        if cpu_request.is_empty() {
            report.warn(
                "market_data_hpa_cpu",
                object,
                "HPA CPU utilization needs a CPU request; the default is 100m",
            );
        }
        let names = service.spec.symbol_names();
        if names.is_empty() {
            report.error(
                "market_data_symbols",
                object,
                "declare at least one symbol or feed group",
            );
        }
        let mut declared: Vec<&str> = Vec::new();
        for group in &service.spec.feed_groups {
            declared.extend(group.symbols.iter().map(String::as_str));
        }
        declared.extend(service.spec.symbols.iter().map(String::as_str));
        let mut seen = BTreeSet::new();
        for name in declared {
            if !seen.insert(name) {
                report.error(
                    "market_data_symbol_duplicate",
                    object,
                    format!("symbol {name} is declared more than once"),
                );
            }
        }
        for name in &names {
            if !catalog.symbols.contains_key(name) {
                report.error(
                    "market_data_symbol",
                    object,
                    format!("unknown symbol {name}"),
                );
                continue;
            }
            if let Some(deployment) = catalog
                .deployments
                .values()
                .find(|deployment| deployment.spec.symbol_ref == *name)
                && let Some(shard) = catalog.shards.get(&deployment.spec.shard_ref)
                && shard.spec.release.wire_version != service.spec.release.wire_version
            {
                report.error(
                    "market_data_release",
                    object,
                    format!(
                        "market-data wire version {} does not match shard {} wire version {}",
                        service.spec.release.wire_version,
                        shard.name,
                        shard.spec.release.wire_version
                    ),
                );
            }
        }

        let mut group_names = BTreeSet::new();
        let mut stream_ranges: Vec<(u16, u16, &str, &str)> = Vec::new();
        let service_channel = service.spec.transport.channel.as_deref().unwrap_or("");
        for group in &service.spec.effective_feed_groups() {
            if group.name.trim().is_empty() || !group_names.insert(group.name.clone()) {
                report.error(
                    "market_data_feed_group",
                    object,
                    "feed group names must be non-empty and unique",
                );
            }
            if group.symbols.is_empty() {
                report.warn(
                    "market_data_feed_group_empty",
                    object,
                    format!("feed group {} declares no symbols", group.name),
                );
            }
            if service.spec.transport.mode == MarketDataTransportMode::Nats
                && group.symbols.len() > 1
            {
                report.error(
                    "market_data_nats_group",
                    object,
                    format!(
                        "NATS feed group {} must contain exactly one symbol; the subject has no symbol token",
                        group.name
                    ),
                );
            }
            let first = group.stream_base;
            let last = first.saturating_add(9);
            // NATS subjects include the feed-group name, so two groups with
            // the same stream base do not collide unless their names match
            // (names are already checked to be unique above).
            let channel = if service.spec.transport.mode == MarketDataTransportMode::Multicast {
                group.channel.as_deref().unwrap_or(service_channel)
            } else {
                group.name.as_str()
            };
            if first < 2001 || last == u16::MAX {
                report.error(
                    "market_data_stream_base",
                    object,
                    format!(
                        "feed group {} stream base {first} is outside the engine range (>= 2001)",
                        group.name
                    ),
                );
            }
            for (other_first, other_last, other_name, other_channel) in &stream_ranges {
                if channel == *other_channel && first <= *other_last && *other_first <= last {
                    report.error(
                        "market_data_stream_overlap",
                        object,
                        format!(
                            "feed group {} streams {first}-{last} on {channel} overlap {} streams {other_first}-{other_last}",
                            group.name, other_name
                        ),
                    );
                }
            }
            stream_ranges.push((first, last, group.name.as_str(), channel));
        }

        match service.spec.transport.mode {
            MarketDataTransportMode::Multicast => {
                let groups = service.spec.effective_feed_groups();
                let mut channels: Vec<&str> = Vec::new();
                for group in &groups {
                    match group
                        .channel
                        .as_deref()
                        .or(service.spec.transport.channel.as_deref())
                    {
                        Some(channel) => {
                            if !channels.contains(&channel) {
                                channels.push(channel);
                            }
                        }
                        None => report.error(
                            "market_data_channel",
                            object,
                            format!(
                                "feed group {} needs its own channel or spec.transport.channel",
                                group.name
                            ),
                        ),
                    }
                }
                if channels.is_empty() && groups.is_empty() {
                    report.error(
                        "market_data_channel",
                        object,
                        "multicast transport requires spec.transport.channel",
                    );
                }
                for channel in channels {
                    let Some((host, _port)) = aeron_channel_endpoint(channel) else {
                        report.error(
                            "market_data_channel",
                            object,
                            format!("cannot parse endpoint from Aeron channel {channel}"),
                        );
                        continue;
                    };
                    if !is_multicast_ipv4(&host) {
                        report.warn(
                            "market_data_channel_unicast",
                            object,
                            format!(
                                "{host} is not an IPv4 multicast address; Profile A expects 239.0.0.0/8"
                            ),
                        );
                    } else if let Ok(address) = host.parse::<std::net::Ipv4Addr>()
                        && address.octets()[3] % 2 == 0
                    {
                        report.error(
                            "market_data_channel_even",
                            object,
                            format!(
                                "Aeron requires an odd multicast data address; {host} has an even last octet"
                            ),
                        );
                    }
                    if aeron_channel_param(channel, "ttl").is_none() {
                        report.warn(
                            "market_data_channel_ttl",
                            object,
                            "set |ttl=1 (or higher) so multicast leaves the pod network namespace",
                        );
                    }
                    match aeron_channel_param(channel, "interface") {
                        Some("{pod_ip}") => {}
                        Some("eth0") => report.warn(
                            "market_data_channel_interface",
                            object,
                            "Aeron does not resolve interface=eth0 here; use interface={pod_ip} so each pod binds its own address",
                        ),
                        None => report.warn(
                            "market_data_channel_interface",
                            object,
                            "set interface={pod_ip} so each pod binds its own address",
                        ),
                        Some(_) => {}
                    }
                }
            }
            MarketDataTransportMode::Nats => {
                let url = service
                    .spec
                    .transport
                    .nats_url
                    .as_deref()
                    .unwrap_or("")
                    .trim();
                if url.is_empty() {
                    report.error(
                        "market_data_nats_url",
                        object,
                        "NATS transport requires spec.transport.natsUrl",
                    );
                } else if !(url.starts_with("nats://") || url.starts_with("tls://")) {
                    report.error(
                        "market_data_nats_url",
                        object,
                        format!("NATS URL {url} must start with nats:// or tls://"),
                    );
                }
            }
            MarketDataTransportMode::Bridge => {}
        }

        if service.spec.websocket.port == 0 {
            report.error(
                "market_data_ws_port",
                object,
                "websocket.port must be greater than zero",
            );
        } else if let Some(previous) = ws_ports.insert(
            (service.namespace.as_str(), service.spec.websocket.port),
            service.name.as_str(),
        ) {
            report.error(
                "market_data_ws_port_conflict",
                object,
                format!(
                    "websocket port {} is already used by {previous} in namespace {}",
                    service.spec.websocket.port, service.namespace
                ),
            );
        }
        if service.spec.websocket.max_connections == 0 {
            report.error(
                "market_data_ws_connections",
                object,
                "websocket.maxConnections must be greater than zero",
            );
        }
        if service.spec.websocket.max_streams_per_connection == 0 {
            report.error(
                "market_data_ws_streams",
                object,
                "websocket.maxStreamsPerConnection must be greater than zero",
            );
        }
    }

    // The same symbol may be served by both its shard-local fast service and a
    // global terminal service. They must consume the identical engine feed;
    // otherwise the engine would publish on one channel while the other
    // service subscribes elsewhere.
    let mut nats_feeds: BTreeMap<String, (String, u16)> = BTreeMap::new();
    for service in catalog.market_data_services.values() {
        if service.spec.transport.mode != MarketDataTransportMode::Nats {
            continue;
        }
        for group in service.spec.effective_feed_groups() {
            for symbol in &group.symbols {
                if let Some(previous) = nats_feeds.get(symbol) {
                    if previous.1 != group.stream_base {
                        report.error(
                            "market_data_nats_stream_conflict",
                            &service.name,
                            format!(
                                "symbol {symbol} uses NATS stream base {} in {} but {} in {}",
                                previous.1, previous.0, group.stream_base, service.name
                            ),
                        );
                    }
                } else {
                    nats_feeds.insert(symbol.clone(), (service.name.clone(), group.stream_base));
                }
            }
        }
    }

    let mut feeds: BTreeMap<String, (String, String, u16)> = BTreeMap::new();
    for service in catalog.market_data_services.values() {
        if service.spec.transport.mode != MarketDataTransportMode::Multicast {
            continue;
        }
        for group in service.spec.effective_feed_groups() {
            let Some(channel) = group.channel.as_deref().or(service
                .spec
                .transport
                .channel
                .as_deref())
            else {
                continue;
            };
            for symbol in &group.symbols {
                let feed = (service.name.clone(), channel.to_string(), group.stream_base);
                if let Some(previous) = feeds.get(symbol)
                    && (previous.1 != feed.1 || previous.2 != feed.2)
                {
                    report.error(
                        "market_data_feed_conflict",
                        &service.name,
                        format!(
                            "symbol {symbol} is fed by {} on {}:{} but {} declares {}:{}",
                            previous.0, previous.1, previous.2, feed.0, feed.1, feed.2
                        ),
                    );
                } else {
                    feeds.insert(symbol.clone(), feed);
                }
            }
        }
    }
}

fn validate_order_systems(catalog: &Catalog, report: &mut Report) {
    let mut ports: BTreeMap<(&str, u16), &str> = BTreeMap::new();
    for system in catalog.order_systems.values() {
        let object = &system.name;
        if system.name.trim().is_empty()
            || dns_label(&system.name) != system.name
            || system.name.len() > 63
        {
            report.error(
                "order_system_name",
                object,
                "name must be a lowercase DNS label of at most 63 characters",
            );
        }
        if !catalog.cells.contains_key(&system.spec.cell) {
            report.error(
                "order_system_cell",
                object,
                format!("cell {} is not in the catalog", system.spec.cell),
            );
        }
        if system.spec.image.trim().is_empty() {
            report.error("order_system_image", object, "spec.image is required");
        }
        if system.spec.replicas == 0 {
            report.error(
                "order_system_replicas",
                object,
                "replicas must be greater than zero",
            );
        }
        if system.spec.autoscaling.min_replicas == 0 {
            report.error(
                "order_system_min_replicas",
                object,
                "autoscaling.minReplicas must be at least 1",
            );
        }
        if system.spec.autoscaling.max_replicas != 0
            && system.spec.autoscaling.max_replicas < system.spec.min_replicas()
        {
            report.error(
                "order_system_max_replicas",
                object,
                "autoscaling.maxReplicas must be greater than or equal to minReplicas",
            );
        }
        if !(1..=100).contains(&system.spec.autoscaling.target_cpu_utilization) {
            report.error(
                "order_system_target_cpu",
                object,
                "autoscaling.targetCpuUtilization must be between 1 and 100",
            );
        }
        if system.spec.port == 0 {
            report.error(
                "order_system_port",
                object,
                "port must be greater than zero",
            );
        } else if let Some(previous) =
            ports.insert((system.namespace.as_str(), system.spec.port), object)
        {
            report.error(
                "order_system_port_conflict",
                object,
                format!(
                    "port {} is already used by {previous} in namespace {}",
                    system.spec.port, system.namespace
                ),
            );
        }
        if system.spec.targets.is_empty() {
            report.error(
                "order_system_targets",
                object,
                "declare at least one engine/symbol target",
            );
        }
        let mut seen_targets = BTreeSet::new();
        for target in &system.spec.targets {
            if !seen_targets.insert(target.symbol.clone()) {
                report.error(
                    "order_system_target_duplicate",
                    object,
                    format!("symbol {} is declared more than once", target.symbol),
                );
            }
            if !catalog.symbols.contains_key(&target.symbol) {
                report.error(
                    "order_system_symbol",
                    object,
                    format!("unknown symbol {}", target.symbol),
                );
                continue;
            }
            if !catalog
                .deployments
                .values()
                .any(|deployment| deployment.spec.symbol_ref == target.symbol)
            {
                report.error(
                    "order_system_deployment",
                    object,
                    format!("symbol {} has no engine deployment", target.symbol),
                );
            }
        }
        let mut shard_names = BTreeSet::new();
        let mut ranges: Vec<(u64, u64, &str)> = Vec::new();
        for shard in &system.spec.shards {
            if shard.name.trim().is_empty() || !shard_names.insert(shard.name.clone()) {
                report.error(
                    "order_system_shard",
                    object,
                    "shard names must be non-empty and unique",
                );
            }
            if !shard.name.trim().is_empty() && dns_label(&shard.name) != shard.name {
                report.error(
                    "order_system_shard_name",
                    object,
                    format!("shard {} must be a lowercase DNS label", shard.name),
                );
            }
            if shard.user_id_min > shard.user_id_max {
                report.error(
                    "order_system_user_range",
                    object,
                    format!(
                        "shard {} has user_id_min {} greater than user_id_max {}",
                        shard.name, shard.user_id_min, shard.user_id_max
                    ),
                );
            }
            for (other_min, other_max, other_name) in &ranges {
                if shard.user_id_min <= *other_max && *other_min <= shard.user_id_max {
                    report.error(
                        "order_system_user_overlap",
                        object,
                        format!(
                            "shard {} range {}-{} overlaps {other_name} range {other_min}-{other_max}",
                            shard.name, shard.user_id_min, shard.user_id_max
                        ),
                    );
                }
            }
            ranges.push((shard.user_id_min, shard.user_id_max, shard.name.as_str()));
        }
    }
}

fn validate_cell(cell: &Cell, shards: &[&SymbolShard], report: &mut Report) {
    if cell.name.trim().is_empty() {
        report.error("cell_name", "Cell", "name must not be empty");
    }
    if cell.spec.cluster_id.trim().is_empty() {
        report.error("cluster_id", &cell.name, "spec.clusterId is required");
    }
    if cell.spec.capacity.max_shards == 0 {
        report.error(
            "cell_capacity",
            &cell.name,
            "maxShards must be greater than zero",
        );
    }
    if shards.len() as u32 > cell.spec.capacity.max_shards {
        report.error(
            "cell_max_shards",
            &cell.name,
            format!(
                "{} shards exceed the cell limit of {}",
                shards.len(),
                cell.spec.capacity.max_shards
            ),
        );
    }
    for shard in shards {
        if !cell.spec.namespaces.is_empty() && !cell.spec.namespaces.contains(&shard.namespace) {
            report.warn(
                "cell_namespace",
                &shard.name,
                format!(
                    "namespace {} is not listed in cell {} namespaces",
                    shard.namespace, cell.name
                ),
            );
        }
        if !cell.spec.storage_classes.is_empty()
            && !cell
                .spec
                .storage_classes
                .contains(&shard.spec.storage.class_name)
        {
            report.warn(
                "cell_storage_class",
                &shard.name,
                format!(
                    "storage class {} is not offered by cell {}",
                    shard.spec.storage.class_name, cell.name
                ),
            );
        }
    }
}

fn validate_shard(
    shard: &SymbolShard,
    symbols: &BTreeMap<&str, &TradingSymbol>,
    deployments: &[&SymbolDeployment],
    routes: &[&SymbolRoute],
    report: &mut Report,
) {
    let object = shard.name.as_str();
    if dns_label(&shard.name) != shard.name {
        report.error("shard_name", object, "name must be a DNS-1123 label");
    }
    if shard.spec.image.trim().is_empty() {
        report.error("image", object, "spec.image is required");
    } else if !shard.spec.image.contains("@sha256:") {
        report.warn(
            "image_tag",
            object,
            "pin the image by digest in production; a mutable tag cannot gate upgrades",
        );
    }
    if shard.spec.replicas < 3 {
        report.error(
            "replicas",
            object,
            "at least three voters are required for a fault-tolerant shard",
        );
    } else if shard.spec.replicas > 9 {
        report.error(
            "replicas",
            object,
            "more than nine voters is not supported by this demo",
        );
    }
    if shard.spec.raft.wal_fsync.mode == FsyncMode::None && shard.spec.replicas < 4 {
        report.error(
            "min_voters",
            object,
            "RAFT_WAL_FSYNC_MS is unset with fewer than four voters; set walFsync to every/interval or use four replicas",
        );
    } else if shard.spec.raft.wal_fsync.mode != FsyncMode::None && shard.spec.replicas < 4 {
        report.warn(
            "replicas",
            object,
            "four voters are recommended even with fsync enabled",
        );
    }
    if let WalFsync {
        mode: FsyncMode::Interval,
        interval_ms,
    } = shard.spec.raft.wal_fsync
    {
        match interval_ms {
            None | Some(0) => report.error(
                "fsync_interval",
                object,
                "interval fsync needs intervalMs greater than zero",
            ),
            Some(ms) if ms > 60_000 => report.warn(
                "fsync_interval",
                object,
                format!("a {ms} ms fsync interval makes the RPO large"),
            ),
            Some(_) => {}
        }
    }

    let max_processes = shard.spec.packing.max_processes_per_pod;
    if max_processes == 0 {
        report.error(
            "packing",
            object,
            "maxProcessesPerPod must be greater than zero",
        );
    }
    if max_processes > 20 {
        report.error(
            "packing",
            object,
            "more than 20 processes per pod needs measured headroom (see the local test plan)",
        );
    }
    if shard.spec.class == ShardClass::Dedicated && max_processes != 1 {
        report.error(
            "dedicated_packing",
            object,
            "a dedicated shard must set maxProcessesPerPod to 1",
        );
    }
    if shard.spec.packing.mode == PackingMode::Supervisor
        && max_processes > 1
        && !shard.spec.packing.resources_sized_for_aggregate
    {
        report.warn(
            "supervisor_resources",
            object,
            "supervisor mode shares one container CPU/memory limit across its processes; size defaultProcessResources for the aggregate and set packing.resourcesSizedForAggregate=true",
        );
    }
    if shard.spec.packing.log_dir.is_some()
        && (shard.spec.packing.log_max_bytes == 0 || shard.spec.packing.log_max_files == 0)
    {
        report.error(
            "log_rotation",
            object,
            "logMaxBytes and logMaxFiles must be greater than zero",
        );
    }
    if shard.spec.packing.mode != PackingMode::Supervisor
        && (shard.spec.packing.cgroup_root.is_some()
            || shard.spec.packing.child_memory_max.is_some()
            || shard.spec.packing.child_cpu_max.is_some()
            || shard.spec.packing.log_dir.is_some())
    {
        report.warn(
            "supervisor_isolation",
            object,
            "cgroupRoot/child limits/logDir only apply to supervisor mode; containers mode uses the container runtime",
        );
    }
    if shard.spec.class == ShardClass::Dedicated && deployments.len() > 1 {
        report.error(
            "dedicated_symbols",
            object,
            "a dedicated shard hosts exactly one symbol generation",
        );
    }

    // Port range: HTTP base .. base + max - 1, Raft base+1000 .. +1000 + max - 1.
    let base = shard.spec.network.http_base_port;
    if base < 1024 {
        report.error("http_base_port", object, "httpBasePort must be >= 1024");
    }
    let raft_base = base.saturating_add(1000);
    if u32::from(raft_base) + u32::from(max_processes) > 65_535 {
        report.error(
            "port_range",
            object,
            "HTTP + Raft port ranges exceed the TCP port space; lower httpBasePort or maxProcessesPerPod",
        );
    }

    validate_resources(shard, object, report);

    if shard.spec.quotas.per_process.max_events == 0 {
        report.error("max_events", object, "maxEvents must be greater than zero");
    } else if shard.spec.quotas.per_process.max_events > 1_000_000 {
        report.warn(
            "max_events",
            object,
            "a very large event window increases snapshot size and memory",
        );
    }
    if shard.spec.quotas.per_process.max_inflight_writes == 0 {
        report.warn(
            "max_inflight_writes",
            object,
            "0 disables admission rejection; a bounded value protects the shard",
        );
    }
    if let Some(backup) = &shard.spec.backup
        && backup.destination.trim().is_empty()
    {
        report.error("backup", object, "backup destination must not be empty");
    }

    // Slot/port ownership and uniqueness.
    let mut slots: BTreeMap<u16, &str> = BTreeMap::new();
    let mut ports: BTreeMap<u16, &str> = BTreeMap::new();
    for deployment in deployments {
        let dobj = deployment.name.as_str();
        let placement = &deployment.spec.placement;
        if placement.slot >= max_processes {
            report.error(
                "slot_range",
                dobj,
                format!(
                    "slot {} is outside 0..{}",
                    placement.slot,
                    max_processes.saturating_sub(1)
                ),
            );
        }
        let expected = base + placement.slot;
        if placement.http_port != expected {
            report.error(
                "port_convention",
                dobj,
                format!(
                    "httpPort {} does not match httpBasePort {base} + slot {} = {expected}",
                    placement.http_port, placement.slot
                ),
            );
        }
        if let Some(previous) = slots.insert(placement.slot, dobj) {
            report.error(
                "slot_conflict",
                dobj,
                format!("slot {} is also used by {previous}", placement.slot),
            );
        }
        if let Some(previous) = ports.insert(placement.http_port, dobj) {
            report.error(
                "port_conflict",
                dobj,
                format!(
                    "httpPort {} is also used by {previous}",
                    placement.http_port
                ),
            );
        }
        if ports.contains_key(&placement.raft_port()) {
            report.error(
                "raft_port_conflict",
                dobj,
                format!(
                    "raft port {} collides with an HTTP port in the same shard",
                    placement.raft_port()
                ),
            );
        }
        if deployment.spec.shard_ref != shard.name {
            report.error(
                "deployment_shard",
                dobj,
                format!(
                    "deployment is bound to shard {} but is rendered with {}",
                    deployment.spec.shard_ref, shard.name
                ),
            );
        }
        match symbols.get(deployment.spec.symbol_ref.as_str()) {
            None => report.error(
                "deployment_symbol",
                dobj,
                format!("unknown symbol {}", deployment.spec.symbol_ref),
            ),
            Some(symbol) => validate_deployment_symbol(deployment, symbol, shard, report),
        }
    }

    // Routes.
    let mut routed: BTreeSet<&str> = BTreeSet::new();
    for route in routes {
        let robj = route.name.as_str();
        if !routed.insert(route.spec.symbol_ref.as_str()) {
            report.error(
                "route_duplicate",
                robj,
                format!("more than one route for symbol {}", route.spec.symbol_ref),
            );
        }
        if route.name != route.spec.symbol_ref {
            report.warn(
                "route_name",
                robj,
                "name the SymbolRoute after its symbol so lookups stay unambiguous",
            );
        }
        match symbols.get(route.spec.symbol_ref.as_str()) {
            None => report.error("route_symbol", robj, "unknown symbol reference"),
            Some(symbol) => {
                if let Some(deployment) =
                    deployment_by_ref(deployments, &route.spec.active_deployment_ref)
                {
                    if deployment.spec.symbol_ref != symbol.name {
                        report.error(
                            "route_deployment",
                            robj,
                            "active deployment belongs to a different symbol",
                        );
                    }
                } else {
                    report.error(
                        "route_deployment",
                        robj,
                        format!(
                            "unknown active deployment {}",
                            route.spec.active_deployment_ref
                        ),
                    );
                }
            }
        }
        if let Some(previous) = &route.spec.previous_deployment_ref {
            if previous == &route.spec.active_deployment_ref {
                report.error(
                    "route_previous",
                    robj,
                    "previous and active deployment must differ",
                );
            } else if deployment_by_ref(deployments, previous).is_none() {
                report.error(
                    "route_previous",
                    robj,
                    format!("unknown previous deployment {previous}"),
                );
            }
        }
        if route.name == route.spec.symbol_ref
            && !deployments
                .iter()
                .any(|d| d.name == route.spec.active_deployment_ref)
        {
            report.error(
                "route_dangling",
                robj,
                "route points at a deployment outside this shard",
            );
        }
    }
    for symbol in symbols.values() {
        if !deployments.iter().any(|d| d.spec.symbol_ref == symbol.name) {
            report.error(
                "symbol_deployment",
                &symbol.name,
                "symbol has no SymbolDeployment in the shard",
            );
        }
    }
}

fn deployment_by_ref<'a>(
    deployments: &[&'a SymbolDeployment],
    reference: &str,
) -> Option<&'a SymbolDeployment> {
    deployments.iter().find(|d| d.name == reference).copied()
}

fn validate_deployment_symbol(
    deployment: &SymbolDeployment,
    symbol: &TradingSymbol,
    shard: &SymbolShard,
    report: &mut Report,
) {
    let object = deployment.name.as_str();
    if let Some(image) = &deployment.spec.image {
        if image.trim().is_empty() {
            report.error(
                "deployment_image",
                object,
                "image override must not be empty",
            );
        } else if image != &shard.spec.image {
            report.warn(
                "deployment_image",
                object,
                "a per-generation image must stay in the shard's wire/snapshot compatibility set",
            );
        }
    }
    if deployment.spec.generation != symbol.spec.generation {
        report.error(
            "deployment_generation",
            object,
            format!(
                "deployment generation {} does not match symbol generation {}",
                deployment.spec.generation, symbol.spec.generation
            ),
        );
    }
}

fn validate_resources(shard: &SymbolShard, object: &str, report: &mut Report) {
    let label = "defaultProcessResources";
    let resources = &shard.spec.default_process_resources;
    let requests = &resources.requests;
    let limits = &resources.limits;
    match (
        parse_cpu_millis(&requests.cpu),
        parse_cpu_millis(&limits.cpu),
    ) {
        (Ok(0), _) => report.error(
            "cpu_request",
            object,
            format!("{label}.requests.cpu must be greater than zero"),
        ),
        (Ok(request), Ok(limit)) if request > limit => report.error(
            "cpu_limit",
            object,
            format!("{label}.requests.cpu exceeds limits.cpu"),
        ),
        (Err(e), _) | (_, Err(e)) => report.error("cpu_quantity", object, e),
        _ => {}
    }
    match (
        parse_memory_bytes(&requests.memory),
        parse_memory_bytes(&limits.memory),
    ) {
        (Ok(0), _) => report.error(
            "memory_request",
            object,
            format!("{label}.requests.memory must be greater than zero"),
        ),
        (Ok(request), Ok(limit)) if request > limit => report.error(
            "memory_limit",
            object,
            format!("{label}.requests.memory exceeds limits.memory"),
        ),
        (Err(e), _) | (_, Err(e)) => report.error("memory_quantity", object, e),
        _ => {}
    }
    if shard.spec.storage.size.trim().is_empty() {
        report.error("storage", object, "storage.size is required");
    }
}

fn validate_symbol_ids(catalog: &Catalog, report: &mut Report) {
    let mut seen: BTreeMap<(String, u32), &str> = BTreeMap::new();
    for symbol in catalog.symbols.values() {
        // Resolve the cell through any deployment for this symbol.
        let cell = catalog
            .deployments
            .values()
            .find(|d| d.spec.symbol_ref == symbol.name)
            .and_then(|d| catalog.shards.get(&d.spec.shard_ref))
            .map(|s| s.spec.cell.clone());
        let Some(cell) = cell else { continue };
        if let Some(previous) =
            seen.insert((cell.clone(), symbol.spec.symbol_id), symbol.name.as_str())
            && previous != symbol.name.as_str()
        {
            report.error(
                "symbol_id",
                &symbol.name,
                format!(
                    "symbolId {} is already used by {previous} in cell {cell}",
                    symbol.spec.symbol_id
                ),
            );
        }
    }
}

/// Validate symbol fields that do not depend on placement.
pub fn validate_symbol(symbol: &TradingSymbol) -> Report {
    let mut report = Report::default();
    let object = symbol.name.as_str();
    if dns_label(&symbol.name) != symbol.name {
        report.error("symbol_name", object, "name must be a DNS-1123 label");
    }
    for (role, asset) in [("base", &symbol.spec.base), ("quote", &symbol.spec.quote)] {
        if asset.code.trim().is_empty() {
            report.error(
                "asset_code",
                object,
                format!("{role} asset code is required"),
            );
        } else if asset.code.len() > 16 || !asset.code.chars().all(|c| c.is_ascii_alphanumeric()) {
            report.error(
                "asset_code",
                object,
                format!("{role} asset code must be 1..=16 ASCII alphanumerics"),
            );
        }
        if asset.decimals > 18 {
            report.error(
                "asset_decimals",
                object,
                format!("{role} asset decimals must be <= 18"),
            );
        }
    }
    if symbol.spec.price_decimals > 18 {
        report.error("price_decimals", object, "priceDecimals must be <= 18");
    }
    match decimal_places(&symbol.spec.price_tick) {
        None => report.error("price_tick", object, "priceTick must be a decimal number"),
        Some(places) => {
            if !is_positive_decimal(&symbol.spec.price_tick) {
                report.error("price_tick", object, "priceTick must be greater than zero");
            }
            if places > symbol.spec.price_decimals {
                report.error(
                    "price_tick",
                    object,
                    format!(
                        "priceTick has {places} decimals but priceDecimals is {}",
                        symbol.spec.price_decimals
                    ),
                );
            }
        }
    }
    match decimal_places(&symbol.spec.base_lot) {
        None => report.error("base_lot", object, "baseLot must be a decimal number"),
        Some(places) => {
            if !is_positive_decimal(&symbol.spec.base_lot) {
                report.error("base_lot", object, "baseLot must be greater than zero");
            }
            if places > symbol.spec.base.decimals {
                report.error(
                    "base_lot",
                    object,
                    format!(
                        "baseLot has {places} decimals but the base asset has {}",
                        symbol.spec.base.decimals
                    ),
                );
            }
        }
    }
    if symbol.spec.base.class.is_equity_like()
        && symbol.spec.fee_asset_policy != FeeAssetPolicy::Quote
    {
        report.error(
            "fee_asset_policy",
            object,
            "ETF/equity bases force feeAssetPolicy=quote (brokerage fees are charged in cash)",
        );
    }
    if symbol.spec.generation == 0 {
        report.error("generation", object, "generation starts at 1");
    }
    if symbol.spec.base.class == AssetClass::Unknown
        || symbol.spec.quote.class == AssetClass::Unknown
    {
        report.warn(
            "asset_class",
            object,
            "asset class is unknown; downstream fee/settlement logic will treat it as legacy",
        );
    }
    report
}

/// Per-pod totals for a shard's deployments, using their per-process resources.
pub fn per_pod_totals(shard: &SymbolShard, deployments: &[&SymbolDeployment]) -> PodTotals {
    let mut totals = PodTotals {
        storage_request_bytes: parse_memory_bytes(&shard.spec.storage.size).unwrap_or(0),
        ..Default::default()
    };
    for deployment in deployments {
        let resources = deployment
            .spec
            .resources
            .as_ref()
            .unwrap_or(&shard.spec.default_process_resources);
        totals.processes += 1;
        totals.cpu_request_millis += parse_cpu_millis(&resources.requests.cpu).unwrap_or(0);
        totals.memory_request_bytes += parse_memory_bytes(&resources.requests.memory).unwrap_or(0);
    }
    totals
}

// ---------------------------------------------------------------------------
// Quantity and decimal helpers
// ---------------------------------------------------------------------------

pub fn parse_cpu_millis(input: &str) -> Result<u64, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("CPU quantity is empty".to_string());
    }
    let value = if let Some(millis) = raw.strip_suffix('m') {
        millis
            .parse::<u64>()
            .map_err(|_| format!("invalid CPU quantity {input:?}"))?
    } else {
        let cores = raw
            .parse::<f64>()
            .map_err(|_| format!("invalid CPU quantity {input:?}"))?;
        (cores * 1000.0).round() as u64
    };
    Ok(value)
}

pub fn parse_memory_bytes(input: &str) -> Result<u64, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("memory quantity is empty".to_string());
    }
    let (number, multiplier) = if let Some(v) = raw.strip_suffix("Ki") {
        (v, 1024u64)
    } else if let Some(v) = raw.strip_suffix("Mi") {
        (v, 1024u64.pow(2))
    } else if let Some(v) = raw.strip_suffix("Gi") {
        (v, 1024u64.pow(3))
    } else if let Some(v) = raw.strip_suffix("Ti") {
        (v, 1024u64.pow(4))
    } else if let Some(v) = raw.strip_suffix('K') {
        (v, 1000)
    } else if let Some(v) = raw.strip_suffix('M') {
        (v, 1_000_000)
    } else if let Some(v) = raw.strip_suffix('G') {
        (v, 1_000_000_000)
    } else {
        (raw, 1)
    };
    let value = number
        .parse::<f64>()
        .map_err(|_| format!("invalid memory quantity {input:?}"))?;
    Ok((value * multiplier as f64).round() as u64)
}

/// Number of fractional digits for a plain decimal string (`"0.00100"` -> `Some(5)`).
pub fn decimal_places(input: &str) -> Option<u8> {
    let raw = input.trim();
    if raw.is_empty() || raw.starts_with('-') || raw.starts_with('+') {
        return None;
    }
    let mut parts = raw.split('.');
    let whole = parts.next()?;
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match parts.next() {
        None => Some(0),
        Some(fraction) => {
            if parts.next().is_some()
                || fraction.is_empty()
                || !fraction.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            u8::try_from(fraction.len()).ok()
        }
    }
}

pub fn is_positive_decimal(input: &str) -> bool {
    input.trim().chars().any(|c| c.is_ascii_digit() && c != '0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::demo_bundles;

    #[test]
    fn quantity_parsing() {
        assert_eq!(parse_cpu_millis("250m").unwrap(), 250);
        assert_eq!(parse_cpu_millis("2").unwrap(), 2000);
        assert_eq!(parse_cpu_millis("0.5").unwrap(), 500);
        assert_eq!(parse_memory_bytes("512Mi").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("1Gi").unwrap(), 1024u64.pow(3));
        assert!(parse_cpu_millis("").is_err());
        assert!(parse_memory_bytes("lots").is_err());
    }

    #[test]
    fn order_system_autoscaling_is_validated() {
        use crate::model::{
            OrderSystem, OrderSystemAutoscaling, OrderSystemShard, OrderSystemSpec,
        };

        let mut catalog = crate::store::demo_catalog();
        let system = OrderSystem {
            name: "orders".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: OrderSystemSpec {
                cell: "eu-1".to_string(),
                image: "matchengine-order-system:arm64-v2".to_string(),
                replicas: 1,
                autoscaling: OrderSystemAutoscaling {
                    min_replicas: 0,
                    max_replicas: 0,
                    target_cpu_utilization: 0,
                },
                port: 24300,
                targets: vec![crate::model::OrderSystemTarget {
                    symbol: "eurusd".to_string(),
                    label: "EUR/USD".to_string(),
                    product: Default::default(),
                }],
                shards: vec![OrderSystemShard {
                    name: "users-0".to_string(),
                    user_id_min: 0,
                    user_id_max: 999,
                }],
                ingress: String::new(),
                tls_secret: String::new(),
                resources: None,
                paused: false,
            },
            status: Default::default(),
        };
        catalog.order_systems.insert(system.name.clone(), system);
        let report = validate_catalog(&catalog);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.code == "order_system_min_replicas"),
            "{}",
            report.to_text()
        );
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.code == "order_system_target_cpu"),
            "{}",
            report.to_text()
        );

        let autoscaling = &mut catalog
            .order_systems
            .get_mut("orders")
            .unwrap()
            .spec
            .autoscaling;
        autoscaling.min_replicas = 2;
        autoscaling.max_replicas = 1;
        autoscaling.target_cpu_utilization = 70;
        let report = validate_catalog(&catalog);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.code == "order_system_max_replicas"),
            "{}",
            report.to_text()
        );
    }

    #[test]
    fn decimal_helpers() {
        assert_eq!(decimal_places("0.00001"), Some(5));
        assert_eq!(decimal_places("1"), Some(0));
        assert_eq!(decimal_places("1."), None);
        assert_eq!(decimal_places("1.2.3"), None);
        assert!(is_positive_decimal("0.10"));
        assert!(!is_positive_decimal("0.000"));
    }

    #[test]
    fn scoped_report_keeps_only_matching_objects() {
        let mut report = Report::default();
        report.error("a", "fx-eu-01", "error on the shard");
        report.warn("b", "me-btcusdt", "warning on the other shard");
        report.warn("c", "eurusd", "warning on a symbol");
        let scope = BTreeSet::from(["fx-eu-01".to_string(), "eurusd".to_string()]);
        let scoped = report.scoped(&scope);
        assert_eq!(scoped.errors.len(), 1);
        assert_eq!(scoped.warnings.len(), 1);
        assert_eq!(scoped.warnings[0].object, "eurusd");
        // The original report is unchanged.
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.warnings.len(), 2);
    }

    #[test]
    fn demo_bundles_validate() {
        for bundle in demo_bundles() {
            let report = validate_bundle(&bundle);
            assert!(
                report.is_ok(),
                "{} failed validation: {}",
                bundle.shard.name,
                report.to_text()
            );
        }
    }

    #[test]
    fn slot_and_port_conflicts_are_rejected() {
        let mut bundle = demo_bundles().remove(0);
        let mut duplicate = bundle.deployments[1].clone();
        duplicate.name = "gbpusd-dup".to_string();
        bundle.deployments.push(duplicate);
        let report = validate_bundle(&bundle);
        let codes: Vec<&str> = report.errors.iter().map(|e| e.code.as_str()).collect();
        assert!(codes.contains(&"slot_conflict"), "{}", report.to_text());
        assert!(codes.contains(&"port_conflict"), "{}", report.to_text());
    }

    #[test]
    fn port_convention_is_enforced() {
        let mut bundle = demo_bundles().remove(0);
        bundle.deployments[0].spec.placement.http_port = 21009;
        let report = validate_bundle(&bundle);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.code == "port_convention" && e.object == "eurusd-g1"),
            "{}",
            report.to_text()
        );
    }

    #[test]
    fn no_fsync_requires_four_voters() {
        let mut bundle = demo_bundles().remove(0);
        bundle.shard.spec.replicas = 3;
        bundle.shard.spec.raft.wal_fsync = WalFsync::none();
        let report = validate_bundle(&bundle);
        assert!(
            report.errors.iter().any(|e| e.code == "min_voters"),
            "{}",
            report.to_text()
        );
        bundle.shard.spec.raft.wal_fsync = WalFsync::every();
        assert!(validate_bundle(&bundle).is_ok());
    }

    #[test]
    fn equity_base_requires_quote_fees() {
        let mut bundle = demo_bundles().remove(0);
        bundle.symbols[0].spec.base.class = AssetClass::Equity;
        bundle.symbols[0].spec.fee_asset_policy = FeeAssetPolicy::Received;
        let report = validate_bundle(&bundle);
        assert!(
            report.errors.iter().any(|e| e.code == "fee_asset_policy"),
            "{}",
            report.to_text()
        );
    }

    #[test]
    fn tick_precision_cannot_exceed_price_decimals() {
        let mut bundle = demo_bundles().remove(0);
        bundle.symbols[0].spec.price_decimals = 2;
        bundle.symbols[0].spec.price_tick = "0.00001".to_string();
        let report = validate_bundle(&bundle);
        assert!(
            report.errors.iter().any(|e| e.code == "price_tick"),
            "{}",
            report.to_text()
        );
    }

    #[test]
    fn supervisor_packing_is_supported() {
        let mut bundle = demo_bundles().remove(0);
        bundle.shard.spec.packing.mode = PackingMode::Supervisor;
        let report = validate_bundle(&bundle);
        assert!(
            !report.errors.iter().any(|e| e.code == "packing_mode"),
            "{}",
            report.to_text()
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.code == "supervisor_resources"),
            "{}",
            report.to_text()
        );
        bundle.shard.spec.packing.resources_sized_for_aggregate = true;
        let report = validate_bundle(&bundle);
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.code == "supervisor_resources"),
            "{}",
            report.to_text()
        );
    }

    #[test]
    fn dedicated_shard_has_one_process_per_pod() {
        let mut bundle = demo_bundles()
            .into_iter()
            .find(|bundle| bundle.shard.name == "me-btcusdt")
            .expect("dedicated demo shard");
        bundle.shard.spec.packing.max_processes_per_pod = 4;
        let report = validate_bundle(&bundle);
        assert!(
            report.errors.iter().any(|e| e.code == "dedicated_packing"),
            "{}",
            report.to_text()
        );
    }

    fn market_data_service() -> crate::model::MarketDataService {
        use crate::model::{
            MarketDataFeedGroup, MarketDataService, MarketDataServiceSpec, MarketDataTransport,
        };
        MarketDataService {
            name: "btc-marketdata".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: MarketDataServiceSpec {
                history: None,
                cell: "eu-1".to_string(),
                symbols: Vec::new(),
                feed_groups: vec![MarketDataFeedGroup {
                    name: "btc".to_string(),
                    symbols: vec!["btcusdt".to_string()],
                    stream_base: 2001,
                    channel: None,
                }],
                image: "registry.example.com/matchengine-marketdata@sha256:abc".to_string(),
                release: Default::default(),
                replicas: 2,
                transport: MarketDataTransport {
                    channel: Some(
                        "aeron:udp?endpoint=239.1.2.51:24001|interface={pod_ip}|ttl=1".to_string(),
                    ),
                    ..Default::default()
                },
                websocket: Default::default(),
                autoscaling: Default::default(),
                resources: None,
                paused: false,
            },
            status: Default::default(),
        }
    }

    #[test]
    fn market_data_services_validate_channel_and_symbols() {
        let mut catalog = crate::store::demo_catalog();
        catalog
            .market_data_services
            .insert("btc-marketdata".to_string(), market_data_service());
        let report = validate_catalog(&catalog);
        assert!(report.is_ok(), "{}", report.to_text());

        catalog
            .market_data_services
            .get_mut("btc-marketdata")
            .unwrap()
            .spec
            .transport
            .channel = Some("aeron:udp?endpoint=239.1.2.50:24001|interface={pod_ip}".to_string());
        let service = catalog
            .market_data_services
            .get_mut("btc-marketdata")
            .unwrap();
        service
            .spec
            .feed_groups
            .push(crate::model::MarketDataFeedGroup {
                name: "dup".to_string(),
                symbols: vec!["btcusdt".to_string()],
                stream_base: 2001,
                channel: None,
            });
        let report = validate_catalog(&catalog);
        assert!(
            report
                .errors
                .iter()
                .any(|issue| issue.code == "market_data_channel_even"),
            "{}",
            report.to_text()
        );
        assert!(
            report
                .errors
                .iter()
                .any(|issue| issue.code == "market_data_symbol_duplicate"),
            "{}",
            report.to_text()
        );
        assert!(
            report
                .errors
                .iter()
                .any(|issue| issue.code == "market_data_stream_overlap"),
            "{}",
            report.to_text()
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|issue| issue.code == "market_data_channel_ttl"),
            "{}",
            report.to_text()
        );
    }

    #[test]
    fn market_data_nats_requires_a_valid_url() {
        let mut catalog = crate::store::demo_catalog();
        let mut service = market_data_service();
        service.spec.transport.mode = MarketDataTransportMode::Nats;
        service.spec.transport.channel = None;
        service.spec.transport.nats_url = None;
        catalog
            .market_data_services
            .insert("btc-marketdata".to_string(), service);
        let report = validate_catalog(&catalog);
        assert!(
            report
                .errors
                .iter()
                .any(|issue| issue.code == "market_data_nats_url"),
            "{}",
            report.to_text()
        );

        catalog
            .market_data_services
            .get_mut("btc-marketdata")
            .unwrap()
            .spec
            .transport
            .nats_url = Some("http://nats:4222".to_string());
        let report = validate_catalog(&catalog);
        assert!(
            report
                .errors
                .iter()
                .any(|issue| issue.code == "market_data_nats_url"),
            "{}",
            report.to_text()
        );

        catalog
            .market_data_services
            .get_mut("btc-marketdata")
            .unwrap()
            .spec
            .transport
            .nats_url = Some("nats://nats:4222".to_string());
        let report = validate_catalog(&catalog);
        assert!(
            !report
                .errors
                .iter()
                .any(|issue| issue.code == "market_data_nats_url"),
            "{}",
            report.to_text()
        );
    }
}
