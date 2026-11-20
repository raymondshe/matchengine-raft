//! Deterministic rendering of a shard's Kubernetes objects.
//!
//! The output is a JSON object list (valid for `kubectl apply -f <file>` as a `List`). It is a
//! pure function of the catalog so the GUI, the CLI and the future operator all render the same
//! manifests.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bundle::ShardBundle;
use crate::error::ApiError;
use crate::model::{
    Cell, DeploymentState, MarketDataService, MarketDataTransportMode, OrderSystem,
    OrderSystemProduct, PackingMode, RAFT_PORT_OFFSET, Resources, ShardClass, SymbolDeployment,
    SymbolShard, TradingSymbol, aeron_channel_endpoint, dns_label, fingerprint, is_multicast_ipv4,
    port_name,
};
use crate::store::Catalog;
use crate::validate::validate_bundle;

pub const MANAGED_BY: &str = "match-engine-operator";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    #[default]
    Production,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderOptions {
    #[serde(default)]
    pub profile: Profile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_root: Option<bool>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            profile: Profile::Production,
            namespace: None,
            image_pull_policy: None,
            storage_class: None,
            replicas: None,
            non_root: None,
        }
    }
}

struct Ctx<'a> {
    shard: &'a SymbolShard,
    cell: &'a Cell,
    deployments: Vec<(&'a SymbolDeployment, &'a TradingSymbol)>,
    /// Engine-side market-data egress derived from a matching `MarketDataService`.
    market_data: BTreeMap<String, MarketDataEgress>,
    opts: &'a RenderOptions,
    namespace: String,
}

#[derive(Debug, Clone)]
struct MarketDataEgress {
    mode: MarketDataTransportMode,
    channel: Option<String>,
    nats_url: Option<String>,
    nats_subject_prefix: Option<String>,
    stream_base: u16,
}

#[derive(Debug, Clone)]
struct NatsFeed {
    prefix: String,
    stream_base: u16,
}

impl<'a> Ctx<'a> {
    fn local(&self) -> bool {
        self.opts.profile == Profile::Local
    }

    fn replicas(&self) -> u8 {
        self.opts.replicas.unwrap_or(self.shard.spec.replicas)
    }

    fn non_root(&self) -> bool {
        self.opts.non_root.unwrap_or(true)
    }

    fn supervisor(&self) -> bool {
        self.shard.spec.packing.mode == PackingMode::Supervisor
    }

    /// Whether any symbol in this shard publishes through Aeron and therefore
    /// needs the in-memory media-driver volume.
    fn uses_aeron(&self) -> bool {
        self.market_data
            .values()
            .any(|egress| egress.mode == MarketDataTransportMode::Multicast)
    }

    fn storage_class(&self) -> String {
        self.opts
            .storage_class
            .clone()
            .unwrap_or_else(|| self.shard.spec.storage.class_name.clone())
    }

    fn image_pull_policy(&self) -> String {
        self.opts
            .image_pull_policy
            .clone()
            .unwrap_or_else(|| "IfNotPresent".to_string())
    }

    fn http_range(&self) -> (u16, u16) {
        let base = self.shard.spec.network.http_base_port;
        let last = base.saturating_add(
            self.shard
                .spec
                .packing
                .max_processes_per_pod
                .saturating_sub(1),
        );
        (base, last)
    }

    fn raft_range(&self) -> (u16, u16) {
        let (base, last) = self.http_range();
        (
            base.saturating_add(RAFT_PORT_OFFSET),
            last.saturating_add(RAFT_PORT_OFFSET),
        )
    }

    fn labels(&self) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert(
            "app.kubernetes.io/name".to_string(),
            "match-engine".to_string(),
        );
        labels.insert(
            "app.kubernetes.io/instance".to_string(),
            self.shard.name.clone(),
        );
        labels.insert(
            "app.kubernetes.io/managed-by".to_string(),
            MANAGED_BY.to_string(),
        );
        labels.insert("matching.io/shard".to_string(), self.shard.name.clone());
        labels.insert("matching.io/cluster".to_string(), self.shard.name.clone());
        labels.insert("matching.io/cell".to_string(), self.shard.spec.cell.clone());
        labels.insert(
            "matching.io/cluster-id".to_string(),
            self.cell.spec.cluster_id.clone(),
        );
        labels.insert(
            "matching.io/environment".to_string(),
            self.shard.spec.environment.clone(),
        );
        labels
    }

    fn selector_labels(&self) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert(
            "app.kubernetes.io/name".to_string(),
            "match-engine".to_string(),
        );
        labels.insert("matching.io/cluster".to_string(), self.shard.name.clone());
        labels
    }

    fn image(&self, deployment: &SymbolDeployment) -> String {
        deployment
            .spec
            .image
            .clone()
            .unwrap_or_else(|| self.shard.spec.image.clone())
    }

    fn resources(&self, deployment: &SymbolDeployment) -> Resources {
        deployment
            .spec
            .resources
            .clone()
            .unwrap_or_else(|| self.shard.spec.default_process_resources.clone())
    }
}

fn labels_value(labels: &BTreeMap<String, String>) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in labels {
        map.insert(key.clone(), Value::String(value.clone()));
    }
    Value::Object(map)
}

fn metadata(ctx: &Ctx<'_>, name: &str, labels: &BTreeMap<String, String>) -> Value {
    json!({
        "name": name,
        "namespace": ctx.namespace,
        "labels": labels_value(labels),
    })
}

/// Canonical NATS subject prefix (`md.<cell>.<feed-group>`) for one symbol.
///
/// Every NATS `MarketDataService` that covers a symbol must subscribe to the
/// same prefix as the engine publisher. The smallest service wins (the same
/// rank rule used for the engine egress), so a shard-local fast service and a
/// global terminal service share one subject tree even when their local feed
/// group names differ.
fn nats_feed(catalog: &Catalog, cell: &str, symbol: &str) -> Option<NatsFeed> {
    let mut best: Option<(usize, String, String, String, u16)> = None;
    for service in catalog.market_data_services.values() {
        if service.spec.paused
            || service.spec.cell != cell
            || service.spec.transport.mode != MarketDataTransportMode::Nats
        {
            continue;
        }
        let base = service
            .spec
            .transport
            .subject_prefix
            .trim()
            .trim_end_matches('.');
        let base = if base.is_empty() { "md" } else { base };
        // Only services that actively serve the symbol take part in the
        // canonical-subject ranking; a stopped symbol leaves the feed entirely.
        let active = catalog.market_data_symbols(service);
        if !active.iter().any(|name| name == symbol) {
            continue;
        }
        let rank = active.len();
        for group in service.spec.effective_feed_groups() {
            if !group.symbols.iter().any(|name| name == symbol) {
                continue;
            }
            let replace =
                best.as_ref()
                    .is_none_or(|(current_rank, current_name, current_group, _, _)| {
                        (rank, service.name.as_str(), group.name.as_str())
                            < (*current_rank, current_name.as_str(), current_group.as_str())
                    });
            if replace {
                best = Some((
                    rank,
                    service.name.clone(),
                    group.name.clone(),
                    base.to_string(),
                    group.stream_base,
                ));
            }
            break;
        }
    }
    best.map(|(_, _, group, base, stream_base)| NatsFeed {
        prefix: format!("{base}.{cell}.{group}"),
        stream_base,
    })
}

/// Render every object for one shard. Returns 422 with the validation report when the catalog
/// cannot produce a safe manifest set.
pub fn render_shard(
    catalog: &Catalog,
    shard_name: &str,
    opts: &RenderOptions,
) -> Result<Vec<Value>, ApiError> {
    let shard = catalog.shards.get(shard_name).ok_or_else(|| {
        ApiError::not_found("shard_not_found", format!("unknown shard {shard_name}"))
    })?;
    let cell = catalog.cells.get(&shard.spec.cell).ok_or_else(|| {
        ApiError::unprocessable(crate::validate::Report {
            errors: vec![crate::validate::Issue {
                severity: crate::validate::Severity::Error,
                code: "cell_missing".to_string(),
                object: shard.name.clone(),
                message: format!("cell {} is not registered", shard.spec.cell),
            }],
            warnings: Vec::new(),
        })
    })?;

    let deployments: Vec<&SymbolDeployment> = catalog
        .shard_deployments(&shard.name)
        .into_iter()
        .filter(|deployment| deployment.spec.desired_state != DeploymentState::Disabled)
        .collect();
    let empty = deployments.is_empty();
    let supervisor = shard.spec.packing.mode == PackingMode::Supervisor;
    let mut symbols = Vec::new();
    let mut bound_deployments = Vec::new();
    let mut routes = Vec::new();
    for deployment in &deployments {
        let symbol = catalog
            .symbols
            .get(&deployment.spec.symbol_ref)
            .ok_or_else(|| {
                ApiError::unprocessable(crate::validate::Report {
                    errors: vec![crate::validate::Issue {
                        severity: crate::validate::Severity::Error,
                        code: "symbol_missing".to_string(),
                        object: deployment.name.clone(),
                        message: format!("unknown symbol {}", deployment.spec.symbol_ref),
                    }],
                    warnings: Vec::new(),
                })
            })?;
        if let Some(route) = catalog.route_for_symbol(&symbol.name)
            && deployments
                .iter()
                .any(|deployment| deployment.name == route.spec.active_deployment_ref)
        {
            routes.push(route.clone());
        }
        symbols.push(symbol.clone());
        bound_deployments.push((*deployment).clone());
    }
    let bundle = ShardBundle {
        cell: cell.clone(),
        shard: shard.clone(),
        symbols,
        deployments: bound_deployments,
        routes,
    };
    let report = validate_bundle(&bundle);
    if !report.is_ok() {
        return Err(ApiError::unprocessable(report));
    }

    // Engine-side egress is derived from every MarketDataService that covers a
    // symbol in this shard. A shard-specific (smaller) service wins over a
    // global service so adding a cross-shard terminal feed cannot silently
    // re-point an engine publication.
    let mut market_data: BTreeMap<String, (usize, String, MarketDataEgress)> = BTreeMap::new();
    for service in catalog.market_data_services.values() {
        if service.spec.paused || service.spec.transport.mode == MarketDataTransportMode::Bridge {
            continue;
        }
        let rank = catalog.market_data_symbols(service).len();
        for group in service.spec.effective_feed_groups() {
            let channel = group
                .channel
                .as_deref()
                .or(service.spec.transport.channel.as_deref());
            if service.spec.transport.mode == MarketDataTransportMode::Multicast
                && channel.is_none()
            {
                continue;
            }
            for symbol_name in &group.symbols {
                if !deployments
                    .iter()
                    .any(|deployment| deployment.spec.symbol_ref == *symbol_name)
                {
                    continue;
                }
                let candidate = if service.spec.transport.mode == MarketDataTransportMode::Nats {
                    let Some(feed) = nats_feed(catalog, &service.spec.cell, symbol_name) else {
                        continue;
                    };
                    MarketDataEgress {
                        mode: MarketDataTransportMode::Nats,
                        channel: None,
                        nats_url: service.spec.transport.nats_url.clone(),
                        nats_subject_prefix: Some(feed.prefix),
                        stream_base: feed.stream_base,
                    }
                } else {
                    let Some(channel) = channel else {
                        continue;
                    };
                    MarketDataEgress {
                        mode: MarketDataTransportMode::Multicast,
                        channel: Some(channel.to_string()),
                        nats_url: None,
                        nats_subject_prefix: None,
                        stream_base: group.stream_base,
                    }
                };
                let replace =
                    market_data
                        .get(symbol_name)
                        .is_none_or(|(current_rank, current_name, _)| {
                            (rank, service.name.as_str()) < (*current_rank, current_name.as_str())
                        });
                if replace {
                    market_data
                        .insert(symbol_name.clone(), (rank, service.name.clone(), candidate));
                }
            }
        }
    }
    let market_data: BTreeMap<String, MarketDataEgress> = market_data
        .into_iter()
        .map(|(symbol, (_, _, egress))| (symbol, egress))
        .collect();
    let ctx = Ctx {
        shard,
        cell,
        deployments: bundle
            .deployments
            .iter()
            .filter_map(|deployment| {
                bundle
                    .symbols
                    .iter()
                    .find(|s| s.name == deployment.spec.symbol_ref)
                    .map(|symbol| (deployment, symbol))
            })
            .collect(),
        market_data,
        opts,
        namespace: opts
            .namespace
            .clone()
            .unwrap_or_else(|| shard.namespace.clone()),
    };

    let mut objects = vec![config_map(&ctx)];
    if !empty || supervisor {
        objects.push(headless_service(&ctx));
        objects.push(metrics_service(&ctx));
        if shard.spec.class == ShardClass::Dedicated && ctx.deployments.len() == 1 {
            objects.push(leader_service(&ctx));
        }
    }
    objects.push(pod_disruption_budget(&ctx));
    objects.push(network_policy(&ctx));
    if !empty || supervisor {
        objects.push(stateful_set(&ctx));
    }
    Ok(objects)
}

/// Wrap rendered objects in a `v1/List` so `kubectl apply -f` works on one file.
pub fn as_list(items: Vec<Value>) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "List",
        "items": items,
    })
}

fn config_map(ctx: &Ctx<'_>) -> Value {
    let slots: Vec<Value> = ctx
        .deployments
        .iter()
        .map(|(deployment, symbol)| {
            json!({
                "name": symbol.name,
                "deploymentRef": deployment.name,
                "slot": deployment.spec.placement.slot,
                "httpPort": deployment.spec.placement.http_port,
                "raftPort": deployment.spec.placement.raft_port(),
            })
        })
        .collect();
    let body = serde_json::to_string_pretty(&json!({
        "shard": ctx.shard.name,
        "cell": ctx.shard.spec.cell,
        "processes": slots,
    }))
    .unwrap_or_default();
    let supervisor_processes: Vec<Value> = ctx
        .deployments
        .iter()
        .map(|(deployment, symbol)| {
            let mut process = json!({
                "name": symbol.name,
                "slot": deployment.spec.placement.slot,
                "httpPort": deployment.spec.placement.http_port,
                "raftPort": deployment.spec.placement.raft_port(),
                "raftIdOffset": deployment.spec.raft_id_offset,
            });
            if let Some(egress) = ctx.market_data.get(&symbol.name) {
                let mut market_data = json!({
                    "streamBase": egress.stream_base,
                    "heartbeatMs": 1000,
                    "statusEvery": 10,
                });
                if let Some(channel) = &egress.channel {
                    market_data["channel"] = json!(channel);
                    market_data["dir"] =
                        json!(format!("/dev/shm/aeron-{}", dns_label(&symbol.name)));
                    market_data["embedded"] = json!(true);
                }
                if let Some(url) = &egress.nats_url {
                    market_data["natsUrl"] = json!(url);
                }
                if let Some(prefix) = &egress.nats_subject_prefix {
                    market_data["natsSubjectPrefix"] = json!(prefix);
                }
                process["marketData"] = market_data;
            }
            process
        })
        .collect();
    let supervisor_body = serde_json::to_string_pretty(&json!({
        "processes": supervisor_processes,
    }))
    .unwrap_or_default();
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": metadata(ctx, &format!("{}-shard", ctx.shard.name), &ctx.labels()),
        "data": {
            "slots.json": body,
            "processes.json": supervisor_body,
        },
    })
}

fn service_slots(ctx: &Ctx<'_>) -> Vec<u16> {
    if ctx.supervisor() {
        (0..ctx.shard.spec.packing.max_processes_per_pod).collect()
    } else {
        ctx.deployments
            .iter()
            .map(|(deployment, _)| deployment.spec.placement.slot)
            .collect()
    }
}

fn headless_service(ctx: &Ctx<'_>) -> Value {
    let base = ctx.shard.spec.network.http_base_port;
    let ports: Vec<Value> = service_slots(ctx)
        .into_iter()
        .flat_map(|slot| {
            let http = base.saturating_add(slot);
            let raft = http.saturating_add(RAFT_PORT_OFFSET);
            [
                json!({
                    "name": port_name("http", slot),
                    "port": http,
                    "targetPort": port_name("http", slot),
                    "protocol": "TCP",
                }),
                json!({
                    "name": port_name("raft", slot),
                    "port": raft,
                    "targetPort": port_name("raft", slot),
                    "protocol": "TCP",
                }),
            ]
        })
        .collect();
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": metadata(
            ctx,
            &format!("{}-headless", ctx.shard.name),
            &ctx.labels(),
        ),
        "spec": {
            "clusterIP": "None",
            "publishNotReadyAddresses": true,
            "selector": labels_value(&ctx.selector_labels()),
            "ports": ports,
        },
    })
}

fn metrics_service(ctx: &Ctx<'_>) -> Value {
    let base = ctx.shard.spec.network.http_base_port;
    let ports: Vec<Value> = service_slots(ctx)
        .into_iter()
        .map(|slot| {
            json!({
                "name": port_name("metrics", slot),
                "port": base.saturating_add(slot),
                "targetPort": port_name("http", slot),
                "protocol": "TCP",
                "appProtocol": "http",
            })
        })
        .collect();
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": metadata(
            ctx,
            &format!("{}-metrics", ctx.shard.name),
            &ctx.labels(),
        ),
        "spec": {
            "type": "ClusterIP",
            "selector": labels_value(&ctx.selector_labels()),
            "ports": ports,
        },
    })
}

fn leader_service(ctx: &Ctx<'_>) -> Value {
    let Some((deployment, _)) = ctx.deployments.first() else {
        return Value::Null;
    };
    let mut selector = ctx.selector_labels();
    selector.insert("matching.io/role".to_string(), "leader".to_string());
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": metadata(
            ctx,
            &format!("{}-leader", ctx.shard.name),
            &ctx.labels(),
        ),
        "spec": {
            "type": "ClusterIP",
            "selector": labels_value(&selector),
            "ports": [{
                "name": port_name("http", deployment.spec.placement.slot),
                "port": deployment.spec.placement.http_port,
                "targetPort": port_name("http", deployment.spec.placement.slot),
                "protocol": "TCP",
            }],
        },
    })
}

fn pod_disruption_budget(ctx: &Ctx<'_>) -> Value {
    let min_available = ctx.replicas().saturating_sub(1).max(1);
    json!({
        "apiVersion": "policy/v1",
        "kind": "PodDisruptionBudget",
        "metadata": metadata(ctx, &ctx.shard.name, &ctx.labels()),
        "spec": {
            "minAvailable": min_available,
            "selector": { "matchLabels": labels_value(&ctx.selector_labels()) },
        },
    })
}

fn network_policy(ctx: &Ctx<'_>) -> Value {
    let (http_first, http_last) = ctx.http_range();
    let (raft_first, raft_last) = ctx.raft_range();
    // The console's cluster view probes each peer's /console/api/node over HTTP, so shard
    // peers need HTTP ingress/egress in addition to Raft. This stays inside the shard's
    // own trust boundary; external HTTP remains limited to the control-plane namespaces.
    let mut policy = json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": metadata(ctx, &ctx.shard.name, &ctx.labels()),
        "spec": {
            "podSelector": { "matchLabels": labels_value(&ctx.selector_labels()) },
            "policyTypes": ["Ingress", "Egress"],
            "ingress": [
                {
                    "from": [{ "podSelector": { "matchLabels": { "matching.io/cluster": ctx.shard.name } } }],
                    "ports": [{ "protocol": "TCP", "port": raft_first, "endPort": raft_last }],
                },
                {
                    "from": [
                        { "namespaceSelector": { "matchLabels": { "matching.io/role": "gateway" } } },
                        { "namespaceSelector": { "matchLabels": { "matching.io/role": "observability" } } },
                        { "namespaceSelector": { "matchLabels": { "matching.io/role": "control-plane" } } },
                    ],
                    "ports": [{ "protocol": "TCP", "port": http_first, "endPort": http_last }],
                },
                {
                    "from": [{ "podSelector": { "matchLabels": { "matching.io/cluster": ctx.shard.name } } }],
                    "ports": [{ "protocol": "TCP", "port": http_first, "endPort": http_last }],
                },
                {
                    "from": [{ "podSelector": { "matchLabels": {
                        "app.kubernetes.io/name": "match-engine",
                        "matching.io/cluster-id": ctx.cell.spec.cluster_id
                    } } }],
                },
                {
                    "from": [{ "podSelector": { "matchLabels": {
                        "app.kubernetes.io/name": "match-order-system",
                        "matching.io/cell": ctx.cell.name
                    } } }],
                    "ports": [{ "protocol": "TCP", "port": http_first, "endPort": http_last }],
                },
            ],
            "egress": [
                {
                    "to": [{ "podSelector": { "matchLabels": { "matching.io/cluster": ctx.shard.name } } }],
                    "ports": [{ "protocol": "TCP", "port": raft_first, "endPort": raft_last }],
                },
                {
                    "to": [{ "ipBlock": { "cidr": "10.0.0.0/8" } }],
                    "ports": [{ "protocol": "TCP", "port": 443 }],
                },
                {
                    "to": [{ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": "kube-system" } } }],
                    "ports": [
                        { "protocol": "UDP", "port": 53 },
                        { "protocol": "TCP", "port": 53 },
                    ],
                },
                {
                    "to": [{ "podSelector": { "matchLabels": { "matching.io/cluster": ctx.shard.name } } }],
                    "ports": [{ "protocol": "TCP", "port": http_first, "endPort": http_last }],
                },
                {
                    "to": [{ "podSelector": { "matchLabels": {
                        "app.kubernetes.io/name": "match-engine",
                        "matching.io/cluster-id": ctx.cell.spec.cluster_id
                    } } }],
                },
            ],
        },
    });
    // Aeron market-data egress. NetworkPolicy is L3/L4; multicast group
    // destinations are covered by the UDP port rule. Omit the rule entirely
    // when the shard serves no market-data feed.
    let ports = market_data_udp_ports(ctx);
    if !ports.is_empty() {
        // Receivers send Aeron status/NAK control messages back to the
        // multicast control address (group + 1) on the same port. Without this
        // ingress rule the publisher never learns about subscribers and its
        // term buffer fills with backpressure.
        policy["spec"]["ingress"]
            .as_array_mut()
            .expect("ingress array")
            .push(json!({
                "from": [{ "podSelector": { "matchLabels": {
                    "app.kubernetes.io/name": "match-marketdata",
                    "matching.io/cell": ctx.cell.name
                } } }],
                "ports": ports,
            }));
        policy["spec"]["egress"]
            .as_array_mut()
            .expect("egress array")
            .push(json!({
                "to": [{ "ipBlock": { "cidr": "0.0.0.0/0" } }],
                "ports": ports,
            }));
    }
    // Profile N: the engine opens one TCP connection to NATS. The default
    // cluster policy is deny-all egress, so this rule is required.
    if ctx
        .market_data
        .values()
        .any(|egress| egress.mode == MarketDataTransportMode::Nats)
    {
        policy["spec"]["egress"]
            .as_array_mut()
            .expect("egress array")
            .push(json!({
                "to": [{ "ipBlock": { "cidr": "0.0.0.0/0" } }],
                "ports": [{ "protocol": "TCP", "port": 4222 }],
            }));
    }
    policy
}

fn market_data_udp_ports(ctx: &Ctx<'_>) -> Vec<Value> {
    let ports: std::collections::BTreeSet<u16> = ctx
        .market_data
        .values()
        .filter_map(|egress| {
            egress
                .channel
                .as_deref()
                .and_then(aeron_channel_endpoint)
                .map(|(_, port)| port)
        })
        .collect();
    ports
        .into_iter()
        .map(|port| json!({ "protocol": "UDP", "port": port }))
        .collect()
}

fn stateful_set(ctx: &Ctx<'_>) -> Value {
    if ctx.supervisor() {
        return supervisor_stateful_set(ctx);
    }
    let containers: Vec<Value> = ctx
        .deployments
        .iter()
        .map(|(deployment, symbol)| container(ctx, deployment, symbol))
        .collect();

    let pod_labels = ctx.labels();

    let mut volumes = vec![json!({ "name": "tmp", "emptyDir": {} })];
    if ctx.uses_aeron() {
        volumes.push(json!({
            "name": "aeron",
            "emptyDir": { "medium": "Memory", "sizeLimit": "256Mi" },
        }));
    }
    let mut pod_spec = json!({
        "automountServiceAccountToken": false,
        "terminationGracePeriodSeconds": 60,
        "securityContext": pod_security_context(ctx),
        "containers": containers,
        "volumes": volumes,
    });

    if ctx.local() {
        pod_spec["affinity"] = json!({
            "podAntiAffinity": {
                "preferredDuringSchedulingIgnoredDuringExecution": [{
                    "weight": 100,
                    "podAffinityTerm": {
                        "labelSelector": { "matchLabels": labels_value(&ctx.selector_labels()) },
                        "topologyKey": "kubernetes.io/hostname",
                    },
                }],
            },
        });
    } else {
        pod_spec["affinity"] = json!({
            "podAntiAffinity": {
                "requiredDuringSchedulingIgnoredDuringExecution": [{
                    "labelSelector": { "matchLabels": labels_value(&ctx.selector_labels()) },
                    "topologyKey": "kubernetes.io/hostname",
                }],
            },
        });
        pod_spec["topologySpreadConstraints"] = json!([{
            "maxSkew": 1,
            "topologyKey": "topology.kubernetes.io/zone",
            "whenUnsatisfiable": "ScheduleAnyway",
            "labelSelector": { "matchLabels": labels_value(&ctx.selector_labels()) },
        }]);
    }

    json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": metadata(ctx, &ctx.shard.name, &ctx.labels()),
        "spec": {
            "serviceName": format!("{}-headless", ctx.shard.name),
            "replicas": ctx.replicas(),
            "podManagementPolicy": "Parallel",
            "updateStrategy": { "type": "OnDelete" },
            "selector": { "matchLabels": labels_value(&ctx.selector_labels()) },
            "template": {
                "metadata": {
                    "labels": labels_value(&pod_labels),
                    "annotations": {
                        "matching.io/config-render": "k8s_control_plane",
                        "matching.io/release-wire": ctx.shard.spec.release.wire_version.to_string(),
                        "matching.io/release-snapshot": ctx.shard.spec.release.snapshot_version.to_string(),
                        "matching.io/release-mixed-safe": ctx.shard.spec.release.mixed_version_safe.to_string(),
                    },
                },
                "spec": pod_spec,
            },
            "volumeClaimTemplates": [{
                "metadata": { "name": "data" },
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "storageClassName": ctx.storage_class(),
                    "resources": { "requests": { "storage": ctx.shard.spec.storage.size } },
                },
            }],
        },
    })
}

fn resources_json(resources: &Resources) -> Value {
    let mut value = json!({});
    let mut requests = serde_json::Map::new();
    if !resources.requests.cpu.is_empty() {
        requests.insert("cpu".to_string(), json!(resources.requests.cpu));
    }
    if !resources.requests.memory.is_empty() {
        requests.insert("memory".to_string(), json!(resources.requests.memory));
    }
    if !requests.is_empty() {
        value["requests"] = Value::Object(requests);
    }
    let mut limits = serde_json::Map::new();
    if !resources.limits.cpu.is_empty() {
        limits.insert("cpu".to_string(), json!(resources.limits.cpu));
    }
    if !resources.limits.memory.is_empty() {
        limits.insert("memory".to_string(), json!(resources.limits.memory));
    }
    if !limits.is_empty() {
        value["limits"] = Value::Object(limits);
    }
    value
}

fn supervisor_stateful_set(ctx: &Ctx<'_>) -> Value {
    let base = ctx.shard.spec.network.http_base_port;
    let max_processes = ctx.shard.spec.packing.max_processes_per_pod;
    let mut ports = vec![json!({
        "name": "supervisor",
        "containerPort": 9090,
        "protocol": "TCP",
    })];
    for slot in 0..max_processes {
        let http = base.saturating_add(slot);
        let raft = http.saturating_add(RAFT_PORT_OFFSET);
        ports.push(json!({
            "name": port_name("http", slot),
            "containerPort": http,
            "protocol": "TCP",
        }));
        ports.push(json!({
            "name": port_name("raft", slot),
            "containerPort": raft,
            "protocol": "TCP",
        }));
    }

    let snapshot_ms = u64::from(ctx.shard.spec.raft.snapshot.every_minutes) * 60_000;
    let service = format!("{}-headless", ctx.shard.name);
    let mut env = vec![
        json!({ "name": "POD_NAME", "valueFrom": { "fieldRef": { "fieldPath": "metadata.name" } } }),
        json!({ "name": "POD_NAMESPACE", "valueFrom": { "fieldRef": { "fieldPath": "metadata.namespace" } } }),
        json!({
            "name": "POD_ORDINAL",
            "valueFrom": { "fieldRef": { "fieldPath": "metadata.labels['apps.kubernetes.io/pod-index']" } }
        }),
        json!({
            "name": "POD_FQDN",
            "value": format!("$(POD_NAME).{service}.$(POD_NAMESPACE).svc.cluster.local")
        }),
        json!({ "name": "RUST_LOG", "value": "info" }),
        json!({ "name": "POD_IP", "valueFrom": { "fieldRef": { "fieldPath": "status.podIP" } } }),
    ];
    if let Some(value) = ctx.shard.spec.raft.wal_fsync.env_value() {
        env.push(json!({ "name": "RAFT_WAL_FSYNC_MS", "value": value }));
    }

    let mut supervisor_args = vec![
        json!("--config"),
        json!("/etc/match/processes.json"),
        json!("--id"),
        json!("$(POD_ORDINAL)"),
        json!("--pod-fqdn"),
        json!("$(POD_FQDN)"),
        json!("--data-dir"),
        json!("/data"),
        json!("--engine-bin"),
        json!("/usr/local/bin/matchengine-raft"),
        json!("--supervisor-addr"),
        json!("0.0.0.0:9090"),
        json!("--snapshot-interval-ms"),
        json!(snapshot_ms.to_string()),
        json!("--read-policy"),
        json!(ctx.shard.spec.raft.read_policy.as_str()),
        json!("--max-inflight-writes"),
        json!(
            ctx.shard
                .spec
                .quotas
                .per_process
                .max_inflight_writes
                .to_string()
        ),
        json!("--max-events"),
        json!(ctx.shard.spec.quotas.per_process.max_events.to_string()),
        json!("--max-processes"),
        json!(ctx.shard.spec.packing.max_processes_per_pod.to_string()),
    ];
    if let Some(root) = &ctx.shard.spec.packing.cgroup_root {
        supervisor_args.extend([json!("--cgroup-root"), json!(root)]);
    }
    if let Some(max) = &ctx.shard.spec.packing.child_memory_max {
        supervisor_args.extend([json!("--child-memory-max"), json!(max)]);
    }
    if let Some(max) = &ctx.shard.spec.packing.child_cpu_max {
        supervisor_args.extend([json!("--child-cpu-max"), json!(max)]);
    }
    if let Some(dir) = &ctx.shard.spec.packing.log_dir {
        supervisor_args.extend([
            json!("--log-dir"),
            json!(dir),
            json!("--log-max-bytes"),
            json!(ctx.shard.spec.packing.log_max_bytes.to_string()),
            json!("--log-max-files"),
            json!(ctx.shard.spec.packing.log_max_files.to_string()),
        ]);
    }

    let supervisor = json!({
        "name": "supervisor",
        "image": ctx.shard.spec.image,
        "imagePullPolicy": ctx.image_pull_policy(),
        "command": ["/usr/local/bin/match_supervisor"],
        "args": supervisor_args,
        "env": env,
        "ports": ports,
        "resources": resources_json(&ctx.shard.spec.default_process_resources),
        "securityContext": {
            "allowPrivilegeEscalation": false,
            "capabilities": { "drop": ["ALL"] },
            "readOnlyRootFilesystem": true,
        },
        "startupProbe": {
            "httpGet": { "path": "/health", "port": "supervisor" },
            "failureThreshold": 60,
            "periodSeconds": 5,
        },
        "readinessProbe": {
            "httpGet": { "path": "/ready", "port": "supervisor" },
            "failureThreshold": 5,
            "periodSeconds": 2,
            "timeoutSeconds": 3,
        },
        "livenessProbe": {
            "httpGet": { "path": "/health", "port": "supervisor" },
            "failureThreshold": 6,
            "periodSeconds": 10,
            "timeoutSeconds": 3,
        },
        "volumeMounts": supervisor_volume_mounts(ctx),
    });

    let pod_labels = ctx.labels();
    let mut volumes = vec![
        json!({ "name": "tmp", "emptyDir": {} }),
        json!({
            "name": "config",
            "configMap": {
                "name": format!("{}-shard", ctx.shard.name),
                "items": [{ "key": "processes.json", "path": "processes.json" }],
            },
        }),
    ];
    if ctx.uses_aeron() {
        volumes.push(json!({
            "name": "aeron",
            "emptyDir": { "medium": "Memory", "sizeLimit": "256Mi" },
        }));
    }
    let mut pod_spec = json!({
        "automountServiceAccountToken": false,
        "terminationGracePeriodSeconds": 60,
        "securityContext": pod_security_context(ctx),
        "containers": [supervisor],
        "volumes": volumes,
    });

    if ctx.local() {
        pod_spec["affinity"] = json!({
            "podAntiAffinity": {
                "preferredDuringSchedulingIgnoredDuringExecution": [{
                    "weight": 100,
                    "podAffinityTerm": {
                        "labelSelector": { "matchLabels": labels_value(&ctx.selector_labels()) },
                        "topologyKey": "kubernetes.io/hostname",
                    },
                }],
            },
        });
    } else {
        pod_spec["affinity"] = json!({
            "podAntiAffinity": {
                "requiredDuringSchedulingIgnoredDuringExecution": [{
                    "labelSelector": { "matchLabels": labels_value(&ctx.selector_labels()) },
                    "topologyKey": "kubernetes.io/hostname",
                }],
            },
        });
        pod_spec["topologySpreadConstraints"] = json!([{
            "maxSkew": 1,
            "topologyKey": "topology.kubernetes.io/zone",
            "whenUnsatisfiable": "ScheduleAnyway",
            "labelSelector": { "matchLabels": labels_value(&ctx.selector_labels()) },
        }]);
    }

    json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": metadata(ctx, &ctx.shard.name, &ctx.labels()),
        "spec": {
            "serviceName": format!("{}-headless", ctx.shard.name),
            "replicas": ctx.replicas(),
            "podManagementPolicy": "Parallel",
            "updateStrategy": { "type": "OnDelete" },
            "selector": { "matchLabels": labels_value(&ctx.selector_labels()) },
            "template": {
                "metadata": {
                    "labels": labels_value(&pod_labels),
                    "annotations": {
                        "matching.io/config-render": "k8s_control_plane",
                        "matching.io/release-wire": ctx.shard.spec.release.wire_version.to_string(),
                        "matching.io/release-snapshot": ctx.shard.spec.release.snapshot_version.to_string(),
                        "matching.io/release-mixed-safe": ctx.shard.spec.release.mixed_version_safe.to_string(),
                    },
                },
                "spec": pod_spec,
            },
            "volumeClaimTemplates": [{
                "metadata": { "name": "data" },
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "storageClassName": ctx.storage_class(),
                    "resources": { "requests": { "storage": ctx.shard.spec.storage.size } },
                },
            }],
        },
    })
}

fn supervisor_volume_mounts(ctx: &Ctx<'_>) -> Vec<Value> {
    let mut mounts = vec![
        json!({ "name": "data", "mountPath": "/data" }),
        json!({ "name": "tmp", "mountPath": "/tmp" }),
        json!({ "name": "config", "mountPath": "/etc/match", "readOnly": true }),
    ];
    if ctx.uses_aeron() {
        mounts.push(json!({ "name": "aeron", "mountPath": "/dev/shm" }));
    }
    mounts
}

fn pod_security_context(ctx: &Ctx<'_>) -> Value {
    if ctx.non_root() {
        json!({
            "runAsNonRoot": true,
            "runAsUser": 10001,
            "runAsGroup": 10001,
            "fsGroup": 10001,
            "fsGroupChangePolicy": "OnRootMismatch",
            "seccompProfile": { "type": "RuntimeDefault" },
        })
    } else {
        json!({ "seccompProfile": { "type": "RuntimeDefault" } })
    }
}

fn container(ctx: &Ctx<'_>, deployment: &SymbolDeployment, symbol: &TradingSymbol) -> Value {
    let slot = deployment.spec.placement.slot;
    let http_port = deployment.spec.placement.http_port;
    let snapshot_ms = u64::from(ctx.shard.spec.raft.snapshot.every_minutes) * 60_000;
    let resources = ctx.resources(deployment);
    let service = format!("{}-headless", ctx.shard.name);
    let mut env = vec![
        json!({ "name": "POD_NAME", "valueFrom": { "fieldRef": { "fieldPath": "metadata.name" } } }),
        json!({ "name": "POD_NAMESPACE", "valueFrom": { "fieldRef": { "fieldPath": "metadata.namespace" } } }),
        json!({
            "name": "POD_ORDINAL",
            "valueFrom": { "fieldRef": { "fieldPath": "metadata.labels['apps.kubernetes.io/pod-index']" } }
        }),
        json!({
            "name": "POD_FQDN",
            "value": format!("$(POD_NAME).{service}.$(POD_NAMESPACE).svc.cluster.local")
        }),
        json!({ "name": "RUST_LOG", "value": "info" }),
        json!({ "name": "POD_IP", "valueFrom": { "fieldRef": { "fieldPath": "status.podIP" } } }),
    ];
    if let Some(value) = ctx.shard.spec.raft.wal_fsync.env_value() {
        env.push(json!({ "name": "RAFT_WAL_FSYNC_MS", "value": value }));
    }

    let mut args = vec![
        json!("--id"),
        json!("$(POD_ORDINAL)"),
        json!("--id-offset"),
        json!(deployment.spec.raft_id_offset.to_string()),
        json!("--http-addr"),
        json!(format!("$(POD_FQDN):{http_port}")),
        json!("--data-dir"),
        json!("/data"),
        json!("--instance-prefix"),
        json!(dns_label(&symbol.name)),
        json!("--snapshot-interval-ms"),
        json!(snapshot_ms.to_string()),
        json!("--read-policy"),
        json!(ctx.shard.spec.raft.read_policy.as_str()),
        json!("--max-inflight-writes"),
        json!(
            ctx.shard
                .spec
                .quotas
                .per_process
                .max_inflight_writes
                .to_string()
        ),
        json!("--max-events"),
        json!(ctx.shard.spec.quotas.per_process.max_events.to_string()),
    ];
    if let Some(egress) = ctx.market_data.get(&symbol.name) {
        if let Some(url) = &egress.nats_url {
            args.extend([
                json!("--market-data-nats-url"),
                json!(url),
                json!("--market-data-nats-subject-prefix"),
                json!(egress.nats_subject_prefix.clone().unwrap_or_default()),
                json!("--market-data-nats-stream-base"),
                json!(egress.stream_base.to_string()),
                json!("--market-data-aeron-heartbeat-ms"),
                json!("1000"),
                json!("--market-data-status-every"),
                json!("10"),
            ]);
        } else if let Some(channel) = &egress.channel {
            args.extend([
                json!("--market-data-aeron-channel"),
                json!(channel.replace("{pod_ip}", "$(POD_IP)")),
                json!("--market-data-aeron-embedded-driver"),
                json!("--market-data-aeron-dir"),
                json!(format!("/dev/shm/aeron-{}", dns_label(&symbol.name))),
                json!("--market-data-aeron-stream-base"),
                json!(egress.stream_base.to_string()),
                json!("--market-data-aeron-heartbeat-ms"),
                json!("1000"),
                json!("--market-data-status-every"),
                json!("10"),
            ]);
        }
    }
    let mut volume_mounts = vec![
        json!({ "name": "data", "mountPath": "/data" }),
        json!({ "name": "tmp", "mountPath": "/tmp" }),
    ];
    if ctx
        .market_data
        .get(&symbol.name)
        .is_some_and(|egress| egress.mode == MarketDataTransportMode::Multicast)
    {
        volume_mounts.push(json!({ "name": "aeron", "mountPath": "/dev/shm" }));
    }

    json!({
        "name": dns_label(&symbol.name),
        "image": ctx.image(deployment),
        "imagePullPolicy": ctx.image_pull_policy(),
        "command": ["/usr/local/bin/matchengine-raft"],
        "args": args,
        "env": env,
        "ports": [
            { "name": port_name("http", slot), "containerPort": http_port, "protocol": "TCP" },
            { "name": port_name("raft", slot), "containerPort": deployment.spec.placement.raft_port(), "protocol": "TCP" },
        ],
        "resources": {
            "requests": { "cpu": resources.requests.cpu, "memory": resources.requests.memory },
            "limits": { "cpu": resources.limits.cpu, "memory": resources.limits.memory },
        },
        "startupProbe": {
            "httpGet": { "path": "/health", "port": port_name("http", slot) },
            "periodSeconds": 5,
            "failureThreshold": 60,
        },
        "readinessProbe": {
            "httpGet": { "path": "/ready", "port": port_name("http", slot) },
            "periodSeconds": 2,
            "failureThreshold": 5,
        },
        "livenessProbe": {
            "httpGet": { "path": "/health", "port": port_name("http", slot) },
            "periodSeconds": 10,
            "failureThreshold": 6,
        },
        "securityContext": {
            "allowPrivilegeEscalation": false,
            "readOnlyRootFilesystem": true,
            "capabilities": { "drop": ["ALL"] },
        },
        "volumeMounts": volume_mounts,
    })
}

// ---------------------------------------------------------------------------
// Market-data service
// ---------------------------------------------------------------------------

/// Label that ties every rendered object to one `MarketDataService`.
pub const MARKET_DATA_LABEL: &str = "matching.io/market-data";

struct MarketDataCtx<'a> {
    service: &'a MarketDataService,
    cell: &'a Cell,
    namespace: String,
    /// Resolved symbols with the channel and stream base each one uses.
    symbols: Vec<MarketDataSymbol<'a>>,
    /// False when every declared symbol is stopped; the Deployment then scales to zero.
    has_active_symbols: bool,
}

struct MarketDataSymbol<'a> {
    symbol: &'a TradingSymbol,
    stream_base: u16,
    channel: Option<String>,
    nats_subject_prefix: Option<String>,
}

fn market_data_labels(ctx: &MarketDataCtx<'_>) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/name".to_string(),
        "match-marketdata".to_string(),
    );
    labels.insert(
        "app.kubernetes.io/instance".to_string(),
        ctx.service.name.clone(),
    );
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        MANAGED_BY.to_string(),
    );
    labels.insert(MARKET_DATA_LABEL.to_string(), ctx.service.name.clone());
    labels.insert("matching.io/cell".to_string(), ctx.cell.name.clone());
    labels.insert(
        "matching.io/cluster-id".to_string(),
        ctx.cell.spec.cluster_id.clone(),
    );
    labels
}

fn market_data_selector(ctx: &MarketDataCtx<'_>) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/name".to_string(),
        "match-marketdata".to_string(),
    );
    labels.insert(MARKET_DATA_LABEL.to_string(), ctx.service.name.clone());
    labels
}

fn market_data_error(code: &str, object: &str, message: impl Into<String>) -> ApiError {
    ApiError::unprocessable(crate::validate::Report {
        errors: vec![crate::validate::Issue {
            severity: crate::validate::Severity::Error,
            code: code.to_string(),
            object: object.to_string(),
            message: message.into(),
        }],
        warnings: Vec::new(),
    })
}

/// Market-data subscribers bind the wildcard address.
///
/// The engine publisher keeps `interface={pod_ip}` so multicast leaves its pod
/// network namespace. A subscriber pinned to a pod IP creates an
/// interface-specific IGMP membership; when a second subscriber on the same
/// node joins the same group, one of them can miss the flow. Binding the
/// wildcard address lets every subscriber receive a copy and is the multipod
/// behaviour single-node labs and multi-tenant hosts need.
fn aeron_receiver_channel(channel: &str) -> String {
    channel
        .split('|')
        .filter(|part| !part.trim_start().starts_with("interface="))
        .collect::<Vec<_>>()
        .join("|")
}

fn market_data_config(ctx: &MarketDataCtx<'_>) -> Value {
    let groups: Vec<Value> = ctx
        .service
        .spec
        .effective_feed_groups()
        .iter()
        .map(|group| {
            let symbols: Vec<Value> = group
                .symbols
                .iter()
                .filter_map(|name| {
                    ctx.symbols
                        .iter()
                        .find(|entry| &entry.symbol.name == name)
                        .map(|entry| {
                            json!({
                                "name": entry.symbol.name,
                                "symbolId": entry.symbol.spec.symbol_id,
                                "base": entry.symbol.spec.base,
                                "quote": entry.symbol.spec.quote,
                                "streamBase": entry.stream_base,
                                "channel": entry.channel.as_deref().map(aeron_receiver_channel),
                                "natsSubjectPrefix": entry.nats_subject_prefix,
                            })
                        })
                })
                .collect();
            json!({
                "name": group.name,
                "streamBase": group.stream_base,
                "channel": group
                    .channel
                    .clone()
                    .or_else(|| ctx.service.spec.transport.channel.clone())
                    .map(|channel| aeron_receiver_channel(&channel)),
                "symbols": symbols,
            })
        })
        .collect();
    json!({
        "service": ctx.service.name,
        "cell": ctx.cell.name,
        "release": ctx.service.spec.release,
        "feedGroups": groups,
        "transport": ctx.service.spec.transport,
        "websocket": ctx.service.spec.websocket,
        "paused": ctx.service.spec.paused,
    })
}

fn market_data_config_map(ctx: &MarketDataCtx<'_>, config: &Value) -> Value {
    let body = serde_json::to_string_pretty(config).unwrap_or_default();
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": format!("{}-config", ctx.service.name),
            "namespace": ctx.namespace,
            "labels": labels_value(&market_data_labels(ctx)),
        },
        "data": { "marketdata.json": body },
    })
}

fn market_data_resources(ctx: &MarketDataCtx<'_>) -> Resources {
    ctx.service
        .spec
        .resources
        .clone()
        .unwrap_or_else(|| Resources::new("100m", "128Mi", "1000m", "512Mi"))
}

fn market_data_container(ctx: &MarketDataCtx<'_>) -> Value {
    let ws = &ctx.service.spec.websocket;
    let mut args = vec![json!("--addr"), json!(format!("0.0.0.0:{}", ws.port))];
    for entry in &ctx.symbols {
        args.push(json!("--symbol"));
        args.push(json!(format!(
            "{}:{}:{}:{}",
            entry.symbol.name,
            entry.symbol.spec.symbol_id,
            entry.symbol.spec.base.code,
            entry.symbol.spec.quote.code
        )));
    }
    match ctx.service.spec.transport.mode {
        MarketDataTransportMode::Multicast => {
            for entry in &ctx.symbols {
                args.push(json!("--aeron-channel"));
                args.push(json!(aeron_receiver_channel(
                    &entry
                        .channel
                        .as_deref()
                        .unwrap_or_default()
                        .replace("{pod_ip}", "$(POD_IP)")
                )));
            }
            for entry in &ctx.symbols {
                args.push(json!("--aeron-stream-base"));
                args.push(json!(entry.stream_base.to_string()));
            }
            args.extend([
                json!("--aeron-embedded-driver"),
                json!("--aeron-dir"),
                json!("/dev/shm/aeron"),
            ]);
        }
        MarketDataTransportMode::Nats => {
            args.push(json!("--nats-url"));
            args.push(json!(
                ctx.service
                    .spec
                    .transport
                    .nats_url
                    .clone()
                    .unwrap_or_default()
            ));
            for entry in &ctx.symbols {
                args.push(json!("--nats-subject-prefix"));
                args.push(json!(entry.nats_subject_prefix.clone().unwrap_or_default()));
                args.push(json!("--nats-stream-base"));
                args.push(json!(entry.stream_base.to_string()));
            }
        }
        MarketDataTransportMode::Bridge => {}
    }
    args.extend([
        json!("--max-connections"),
        json!(ws.max_connections.to_string()),
        json!("--max-streams"),
        json!(ws.max_streams_per_connection.to_string()),
        json!("--queue"),
        json!(ws.queue.to_string()),
        json!("--fast-max-streams"),
        json!(ws.fast_max_streams.to_string()),
        json!("--terminal-conflation-ms"),
        json!(ws.terminal_conflation_ms.to_string()),
        json!("--heartbeat-timeout-secs"),
        json!(ws.heartbeat_timeout_secs.to_string()),
    ]);
    if ws.fast_only {
        args.push(json!("--fast-only"));
    }
    if ws.extensions {
        args.push(json!("--extensions"));
    }
    let resources = market_data_resources(ctx);
    json!({
        "name": "market-data",
        "image": ctx.service.spec.image,
        "imagePullPolicy": "IfNotPresent",
        "command": ["/usr/local/bin/matchengine-marketdata"],
        "args": args,
        "env": [
            { "name": "RUST_LOG", "value": "info" },
            { "name": "POD_NAME", "valueFrom": { "fieldRef": { "fieldPath": "metadata.name" } } },
            { "name": "POD_NAMESPACE", "valueFrom": { "fieldRef": { "fieldPath": "metadata.namespace" } } },
            { "name": "POD_IP", "valueFrom": { "fieldRef": { "fieldPath": "status.podIP" } } },
        ],
        "ports": [{ "name": "ws", "containerPort": ws.port, "protocol": "TCP" }],
        "resources": resources_json(&resources),
        "startupProbe": {
            "httpGet": { "path": "/health", "port": "ws" },
            "periodSeconds": 3,
            "failureThreshold": 40,
        },
        // Keep the pod Ready even when an upstream symbol feed is stale: the
        // WebSocket endpoint still accepts clients and reports per-symbol
        // readiness in /ready. Feed readiness is a MarketDataService status
        // condition, not a pod-availability gate.
        "readinessProbe": {
            "httpGet": { "path": "/health", "port": "ws" },
            "periodSeconds": 5,
            "failureThreshold": 3,
        },
        "livenessProbe": {
            "httpGet": { "path": "/health", "port": "ws" },
            "periodSeconds": 10,
            "failureThreshold": 6,
        },
        "lifecycle": {
            "preStop": {
                "exec": { "command": ["/bin/sh", "-c", "sleep 10"] }
            }
        },
        "securityContext": {
            "allowPrivilegeEscalation": false,
            "readOnlyRootFilesystem": true,
            "capabilities": { "drop": ["ALL"] },
        },
        "volumeMounts": [
            { "name": "aeron", "mountPath": "/dev/shm" },
            { "name": "config", "mountPath": "/etc/marketdata", "readOnly": true },
            { "name": "tmp", "mountPath": "/tmp" },
        ],
    })
}

fn market_data_deployment(ctx: &MarketDataCtx<'_>, config: &Value) -> Value {
    let labels = market_data_labels(ctx);
    let selector = market_data_selector(ctx);
    let config_hash = fingerprint(config);
    let release = &ctx.service.spec.release;
    let mut deployment = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": ctx.service.name,
            "namespace": ctx.namespace,
            "labels": labels_value(&labels),
            "annotations": { "matching.io/config-hash": &config_hash },
        },
        "spec": {
            "revisionHistoryLimit": 3,
            "minReadySeconds": 5,
            "progressDeadlineSeconds": 600,
            "strategy": {
                "type": "RollingUpdate",
                "rollingUpdate": { "maxUnavailable": 0, "maxSurge": 1 }
            },
            "selector": { "matchLabels": labels_value(&selector) },
            "template": {
                "metadata": {
                    "labels": labels_value(&labels),
                    "annotations": {
                        "matching.io/config-render": "k8s_control_plane",
                        "matching.io/config-hash": &config_hash,
                        "matching.io/release-wire": release.wire_version.to_string(),
                        "matching.io/release-snapshot": release.snapshot_version.to_string(),
                        "matching.io/release-mixed-safe": release.mixed_version_safe.to_string(),
                    },
                },
                "spec": {
                    "automountServiceAccountToken": false,
                    "terminationGracePeriodSeconds": 45,
                    "securityContext": {
                        "runAsNonRoot": true,
                        "runAsUser": 10001,
                        "runAsGroup": 10001,
                        "fsGroup": 10001,
                        "fsGroupChangePolicy": "OnRootMismatch",
                        "seccompProfile": { "type": "RuntimeDefault" },
                    },
                    "affinity": {
                        "podAntiAffinity": {
                            "preferredDuringSchedulingIgnoredDuringExecution": [{
                                "weight": 100,
                                "podAffinityTerm": {
                                    "labelSelector": { "matchLabels": labels_value(&selector) },
                                    "topologyKey": "kubernetes.io/hostname",
                                },
                            }],
                        },
                    },
                    "containers": [market_data_container(ctx)],
                    "volumes": [
                        {
                            "name": "aeron",
                            "emptyDir": { "medium": "Memory", "sizeLimit": "256Mi" },
                        },
                        {
                            "name": "config",
                            "configMap": { "name": format!("{}-config", ctx.service.name) },
                        },
                        { "name": "tmp", "emptyDir": {} },
                    ],
                },
            },
        },
    });
    // The HPA owns the replica count; pin it when the service is paused or when
    // every symbol it serves has been stopped.
    if ctx.service.spec.paused || !ctx.has_active_symbols {
        deployment["spec"]["replicas"] = json!(0);
    }
    deployment
}

fn market_data_service(ctx: &MarketDataCtx<'_>) -> Value {
    let ws = &ctx.service.spec.websocket;
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": ctx.service.name,
            "namespace": ctx.namespace,
            "labels": labels_value(&market_data_labels(ctx)),
        },
        "spec": {
            "type": "ClusterIP",
            "selector": labels_value(&market_data_selector(ctx)),
            "ports": [{
                "name": "ws",
                "port": ws.port,
                "targetPort": "ws",
                "protocol": "TCP",
                "appProtocol": "http",
            }],
        },
    })
}

fn market_data_ingress(ctx: &MarketDataCtx<'_>) -> Value {
    let ws = &ctx.service.spec.websocket;
    let host = ws.ingress.trim();
    let mut spec = json!({
        "rules": [{
            "host": host,
            "http": {
                "paths": [{
                    "path": "/",
                    "pathType": "Prefix",
                    "backend": {
                        "service": {
                            "name": ctx.service.name,
                            "port": { "name": "ws" },
                        },
                    },
                }],
            },
        }],
    });
    if !ws.tls_secret.trim().is_empty() {
        spec["tls"] = json!([{
            "hosts": [host],
            "secretName": ws.tls_secret,
        }]);
    }
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "Ingress",
        "metadata": {
            "name": ctx.service.name,
            "namespace": ctx.namespace,
            "labels": labels_value(&market_data_labels(ctx)),
        },
        "spec": spec,
    })
}

fn market_data_pdb(ctx: &MarketDataCtx<'_>) -> Value {
    let min_available = ctx.service.spec.min_replicas().saturating_sub(1);
    json!({
        "apiVersion": "policy/v1",
        "kind": "PodDisruptionBudget",
        "metadata": {
            "name": format!("{}-marketdata", ctx.service.name),
            "namespace": ctx.namespace,
            "labels": labels_value(&market_data_labels(ctx)),
        },
        "spec": {
            "minAvailable": min_available,
            "selector": { "matchLabels": labels_value(&market_data_selector(ctx)) },
        },
    })
}

fn market_data_hpa(ctx: &MarketDataCtx<'_>) -> Value {
    let spec = &ctx.service.spec;
    let min_replicas = spec.min_replicas();
    let max_replicas = spec.max_replicas();
    json!({
        "apiVersion": "autoscaling/v2",
        "kind": "HorizontalPodAutoscaler",
        "metadata": {
            "name": ctx.service.name,
            "namespace": ctx.namespace,
            "labels": labels_value(&market_data_labels(ctx)),
        },
        "spec": {
            "scaleTargetRef": {
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "name": ctx.service.name,
            },
            "minReplicas": min_replicas,
            "maxReplicas": max_replicas,
            "metrics": [{
                "type": "Resource",
                "resource": {
                    "name": "cpu",
                    "target": {
                        "type": "Utilization",
                        "averageUtilization": spec.autoscaling.target_cpu_utilization,
                    },
                },
            }],
            "behavior": {
                "scaleUp": {
                    "stabilizationWindowSeconds": 30,
                    "policies": [{ "type": "Percent", "value": 100, "periodSeconds": 30 }],
                },
                "scaleDown": {
                    "stabilizationWindowSeconds": 300,
                    "policies": [{ "type": "Percent", "value": 50, "periodSeconds": 60 }],
                },
            },
        },
    })
}

fn market_data_network_policy(ctx: &MarketDataCtx<'_>) -> Value {
    let ws = &ctx.service.spec.websocket;
    let transport = &ctx.service.spec.transport;
    let mut egress = vec![json!({
        "to": [{ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": "kube-system" } } }],
        "ports": [
            { "protocol": "UDP", "port": 53 },
            { "protocol": "TCP", "port": 53 },
        ],
    })];
    let mut ingress = vec![json!({
        "from": [
            { "namespaceSelector": { "matchLabels": { "matching.io/role": "gateway" } } },
            { "namespaceSelector": { "matchLabels": { "matching.io/role": "observability" } } },
            { "namespaceSelector": { "matchLabels": { "matching.io/role": "control-plane" } } },
            { "podSelector": { "matchLabels": labels_value(&market_data_selector(ctx)) } },
        ],
        "ports": [{ "protocol": "TCP", "port": ws.port }],
    })];

    match transport.mode {
        MarketDataTransportMode::Multicast => {
            let mut aeron_ports = Vec::new();
            for group in ctx.service.spec.effective_feed_groups() {
                let channel = group.channel.as_deref().or(transport.channel.as_deref());
                if let Some((_, port)) = channel.and_then(aeron_channel_endpoint)
                    && !aeron_ports.contains(&port)
                {
                    aeron_ports.push(port);
                }
            }
            if aeron_ports.is_empty() {
                aeron_ports.push(24001);
            }
            let aeron_port_rules: Vec<Value> = aeron_ports
                .iter()
                .map(|port| json!({ "protocol": "UDP", "port": port }))
                .collect();
            egress.push(json!({
                "to": [{ "ipBlock": { "cidr": "0.0.0.0/0" } }],
                "ports": aeron_port_rules.clone(),
            }));
            // Stream-2010 snapshot requests go directly to an engine pod's HTTP port.
            egress.push(json!({
                "to": [{ "podSelector": { "matchLabels": {
                    "app.kubernetes.io/name": "match-engine",
                    "matching.io/cluster-id": ctx.cell.spec.cluster_id
                } } }],
            }));
            ingress.push(json!({
                "from": [{ "podSelector": { "matchLabels": {
                    "app.kubernetes.io/name": "match-engine",
                    "matching.io/cluster-id": ctx.cell.spec.cluster_id
                } } }],
                "ports": aeron_port_rules,
            }));
        }
        MarketDataTransportMode::Nats => {
            // One TCP connection to the shared NATS service. Core NATS fans
            // out to every replica; no direct engine connection is needed.
            egress.push(json!({
                "to": [{ "ipBlock": { "cidr": "0.0.0.0/0" } }],
                "ports": [{ "protocol": "TCP", "port": 4222 }],
            }));
        }
        MarketDataTransportMode::Bridge => {}
    }

    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
            "name": format!("{}-marketdata", ctx.service.name),
            "namespace": ctx.namespace,
            "labels": labels_value(&market_data_labels(ctx)),
        },
        "spec": {
            "podSelector": { "matchLabels": labels_value(&market_data_selector(ctx)) },
            "policyTypes": ["Ingress", "Egress"],
            "ingress": ingress,
            "egress": egress,
        },
    })
}

/// Render one `MarketDataService` into the fan-out Deployment/Service/PDB/NetworkPolicy/ConfigMap.
///
/// Profile A (direct Aeron multicast) and Profile N (direct NATS) are rendered. The per-pod
/// bridge remains a documented fallback and is rejected until an implementation exists.
///
/// Fails with 422 when a symbol is missing, its shard release differs from the service release,
/// the NATS URL is missing, or the Aeron channel is malformed/unsafe.
pub fn render_market_data_service(
    catalog: &Catalog,
    name: &str,
    namespace: Option<&str>,
) -> Result<Vec<Value>, ApiError> {
    let service = catalog.market_data_services.get(name).ok_or_else(|| {
        ApiError::not_found("market_data_not_found", format!("unknown service {name}"))
    })?;
    let cell = catalog.cells.get(&service.spec.cell).ok_or_else(|| {
        market_data_error(
            "cell_missing",
            name,
            format!("cell {} is not registered", service.spec.cell),
        )
    })?;
    match service.spec.transport.mode {
        MarketDataTransportMode::Multicast => {
            let groups = service.spec.effective_feed_groups();
            let mut channels = Vec::new();
            for group in &groups {
                let channel = group
                    .channel
                    .as_deref()
                    .or(service.spec.transport.channel.as_deref())
                    .ok_or_else(|| {
                        market_data_error(
                            "market_data_channel",
                            name,
                            format!(
                                "feed group {} needs its own channel or spec.transport.channel",
                                group.name
                            ),
                        )
                    })?;
                if !channels.contains(&channel) {
                    channels.push(channel);
                }
            }
            if channels.is_empty() {
                return Err(market_data_error(
                    "market_data_channel",
                    name,
                    "multicast transport requires spec.transport.channel or per-group channels",
                ));
            }
            for channel in channels {
                let Some((host, _)) = aeron_channel_endpoint(channel) else {
                    return Err(market_data_error(
                        "market_data_channel",
                        name,
                        format!("cannot parse endpoint from Aeron channel {channel}"),
                    ));
                };
                if let Ok(address) = host.parse::<std::net::Ipv4Addr>()
                    && is_multicast_ipv4(&host)
                    && address.octets()[3] % 2 == 0
                {
                    return Err(market_data_error(
                        "market_data_channel_even",
                        name,
                        format!(
                            "Aeron requires an odd multicast data address; {host} has an even last octet"
                        ),
                    ));
                }
            }
        }
        MarketDataTransportMode::Nats => {
            if service
                .spec
                .transport
                .nats_url
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
            {
                return Err(market_data_error(
                    "market_data_nats_url",
                    name,
                    "NATS transport requires spec.transport.natsUrl",
                ));
            }
        }
        MarketDataTransportMode::Bridge => {
            return Err(market_data_error(
                "market_data_transport",
                name,
                "the per-pod Aeron bridge is not implemented; use multicast or NATS",
            ));
        }
    }

    let declared = service.spec.symbol_names();
    if declared.is_empty() {
        return Err(market_data_error(
            "market_data_symbols",
            name,
            "declare at least one symbol or feed group",
        ));
    }
    // A disabled engine deployment (stopped symbol) leaves the feed. Resolve every
    // declared symbol so a stale name is still rejected, but only render the
    // active ones into the container and the ConfigMap.
    let active: std::collections::BTreeSet<String> =
        catalog.market_data_symbols(service).into_iter().collect();
    let mut symbols = Vec::new();
    for symbol_name in declared {
        let symbol = catalog.symbols.get(&symbol_name).ok_or_else(|| {
            market_data_error(
                "symbol_missing",
                name,
                format!("unknown symbol {symbol_name}"),
            )
        })?;
        let deployment = catalog
            .deployments
            .values()
            .find(|deployment| deployment.spec.symbol_ref == symbol_name)
            .ok_or_else(|| {
                market_data_error(
                    "deployment_missing",
                    name,
                    format!("symbol {symbol_name} has no engine deployment"),
                )
            })?;
        let shard = catalog
            .shards
            .get(&deployment.spec.shard_ref)
            .ok_or_else(|| {
                market_data_error(
                    "shard_missing",
                    name,
                    format!(
                        "deployment {} references unknown shard {}",
                        deployment.name, deployment.spec.shard_ref
                    ),
                )
            })?;
        if shard.spec.release.wire_version != service.spec.release.wire_version {
            return Err(market_data_error(
                "market_data_release",
                name,
                format!(
                    "market-data wire version {} does not match shard {} wire version {}",
                    service.spec.release.wire_version, shard.name, shard.spec.release.wire_version
                ),
            ));
        }
        if !active.contains(&symbol_name) {
            continue;
        }
        let (channel, nats_prefix, stream_base) = match service.spec.transport.mode {
            MarketDataTransportMode::Multicast => {
                let channel = service.spec.channel_for(&symbol_name).ok_or_else(|| {
                    market_data_error(
                        "market_data_channel",
                        name,
                        format!("symbol {symbol_name} has no feed-group or transport channel"),
                    )
                })?;
                (
                    Some(channel),
                    None,
                    service.spec.stream_base_for(&symbol_name),
                )
            }
            MarketDataTransportMode::Nats => {
                let feed =
                    nats_feed(catalog, &cell.name, &symbol_name).unwrap_or_else(|| NatsFeed {
                        prefix: format!("md.{}.{}", cell.name, symbol_name),
                        stream_base: service.spec.stream_base_for(&symbol_name),
                    });
                (None, Some(feed.prefix), feed.stream_base)
            }
            MarketDataTransportMode::Bridge => {
                (None, None, service.spec.stream_base_for(&symbol_name))
            }
        };
        symbols.push(MarketDataSymbol {
            symbol,
            stream_base,
            channel,
            nats_subject_prefix: nats_prefix,
        });
    }
    let namespace = namespace
        .map(str::to_string)
        .filter(|namespace| !namespace.is_empty())
        .unwrap_or_else(|| {
            if service.namespace.is_empty() {
                cell.namespace.clone()
            } else {
                service.namespace.clone()
            }
        });
    let has_active_symbols = !symbols.is_empty();
    let ctx = MarketDataCtx {
        service,
        cell,
        namespace,
        symbols,
        has_active_symbols,
    };
    let config = market_data_config(&ctx);
    let mut objects = vec![
        market_data_config_map(&ctx, &config),
        market_data_deployment(&ctx, &config),
        market_data_service(&ctx),
        market_data_pdb(&ctx),
        market_data_network_policy(&ctx),
    ];
    if !service.spec.websocket.ingress.trim().is_empty() {
        objects.push(market_data_ingress(&ctx));
    }
    // An HPA would scale a zero-symbol service back up, so omit it until a
    // symbol is started again.
    if !service.spec.paused && ctx.has_active_symbols {
        objects.push(market_data_hpa(&ctx));
    }
    Ok(objects)
}

/// Label that ties every rendered object to one `OrderSystem`.
pub const ORDER_SYSTEM_LABEL: &str = "matching.io/order-system";

struct OrderSystemTargetResolved {
    id: String,
    label: String,
    engine: String,
    engine_id: u64,
    product: OrderSystemProduct,
    /// All Raft members (node id + HTTP addr) for leader discovery/failover.
    engine_members: Vec<(u64, String)>,
}

struct OrderSystemCtx<'a> {
    system: &'a OrderSystem,
    cell: &'a Cell,
    namespace: String,
    targets: Vec<OrderSystemTargetResolved>,
}

fn order_system_labels(ctx: &OrderSystemCtx<'_>, shard: Option<&str>) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/name".to_string(),
        "match-order-system".to_string(),
    );
    labels.insert(
        "app.kubernetes.io/instance".to_string(),
        ctx.system.name.clone(),
    );
    labels.insert(ORDER_SYSTEM_LABEL.to_string(), ctx.system.name.clone());
    labels.insert("matching.io/cell".to_string(), ctx.cell.name.clone());
    labels.insert(
        "matching.io/cluster-id".to_string(),
        ctx.cell.spec.cluster_id.clone(),
    );
    if let Some(shard) = shard {
        labels.insert("matching.io/order-shard".to_string(), shard.to_string());
    }
    labels
}

fn order_system_config(ctx: &OrderSystemCtx<'_>) -> Value {
    let targets: Vec<Value> = ctx
        .targets
        .iter()
        .map(|target| {
            json!({
                "id": target.id,
                "label": target.label,
                "engine": target.engine,
                "engineId": target.engine_id,
                "product": target.product,
                "engineMembers": target
                    .engine_members
                    .iter()
                    .map(|(node_id, addr)| json!({ "nodeId": node_id, "addr": addr }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "targets": targets })
}

fn order_system_config_map(ctx: &OrderSystemCtx<'_>, config: &Value) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": format!("{}-config", ctx.system.name),
            "namespace": ctx.namespace,
            "labels": labels_value(&order_system_labels(ctx, None)),
        },
        "data": {
            "targets.json": serde_json::to_string_pretty(config).unwrap_or_default(),
        },
    })
}

fn order_system_resources(ctx: &OrderSystemCtx<'_>) -> Resources {
    ctx.system
        .spec
        .resources
        .clone()
        .unwrap_or_else(|| Resources::new("100m", "128Mi", "1000m", "512Mi"))
}

fn order_system_deployment(
    ctx: &OrderSystemCtx<'_>,
    shard: &crate::model::OrderSystemShard,
    config: &Value,
) -> Value {
    let spec = &ctx.system.spec;
    let name = dns_label(&format!("{}-{}", ctx.system.name, shard.name));
    let labels = order_system_labels(ctx, Some(&shard.name));
    let resources = order_system_resources(ctx);
    let args = vec![
        json!("--addr"),
        json!(format!("0.0.0.0:{}", spec.port)),
        json!("--targets"),
        json!("/etc/order-system/targets.json"),
        json!("--user-id-min"),
        json!(shard.user_id_min.to_string()),
        json!("--user-id-max"),
        json!(shard.user_id_max.to_string()),
    ];
    let mut deployment = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": name,
            "namespace": ctx.namespace,
            "labels": labels_value(&labels),
        },
        "spec": {
            "strategy": { "type": "RollingUpdate", "rollingUpdate": { "maxUnavailable": 0, "maxSurge": 1 } },
            "selector": { "matchLabels": labels_value(&labels) },
            "template": {
                "metadata": {
                    "labels": labels_value(&labels),
                    "annotations": { "matching.io/config-hash": fingerprint(config) },
                },
                "spec": {
                    "automountServiceAccountToken": false,
                    "terminationGracePeriodSeconds": 20,
                    "securityContext": {
                        "runAsNonRoot": true,
                        "runAsUser": 10001,
                        "runAsGroup": 10001,
                        "fsGroup": 10001,
                        "seccompProfile": { "type": "RuntimeDefault" },
                    },
                    "containers": [{
                        "name": "order-system",
                        "image": spec.image,
                        "imagePullPolicy": "IfNotPresent",
                        "command": ["/usr/local/bin/matchengine-order-system"],
                        "args": args,
                        "env": [
                            { "name": "RUST_LOG", "value": "info,actix_web=warn" },
                            { "name": "POD_NAME", "valueFrom": { "fieldRef": { "fieldPath": "metadata.name" } } },
                            { "name": "POD_NAMESPACE", "valueFrom": { "fieldRef": { "fieldPath": "metadata.namespace" } } },
                        ],
                        "ports": [{ "name": "http", "containerPort": spec.port, "protocol": "TCP" }],
                        "resources": resources_json(&resources),
                        "startupProbe": {
                            "httpGet": { "path": "/health", "port": "http" },
                            "periodSeconds": 3,
                            "failureThreshold": 30,
                        },
                        "readinessProbe": {
                            "httpGet": { "path": "/health", "port": "http" },
                            "periodSeconds": 3,
                            "failureThreshold": 3,
                        },
                        "livenessProbe": {
                            "httpGet": { "path": "/health", "port": "http" },
                            "periodSeconds": 10,
                            "failureThreshold": 6,
                        },
                        "securityContext": {
                            "allowPrivilegeEscalation": false,
                            "readOnlyRootFilesystem": true,
                            "capabilities": { "drop": ["ALL"] },
                        },
                        "volumeMounts": [
                            { "name": "config", "mountPath": "/etc/order-system", "readOnly": true },
                            { "name": "tmp", "mountPath": "/tmp" },
                        ],
                    }],
                    "volumes": [
                        { "name": "config", "configMap": { "name": format!("{}-config", ctx.system.name) } },
                        { "name": "tmp", "emptyDir": {} },
                    ],
                },
            },
        },
    });
    // The HPA owns the replica count; only pin it when the system is paused.
    if spec.paused {
        deployment["spec"]["replicas"] = json!(0);
    }
    deployment
}

fn order_system_service(ctx: &OrderSystemCtx<'_>, shard: &crate::model::OrderSystemShard) -> Value {
    let name = dns_label(&format!("{}-{}", ctx.system.name, shard.name));
    let labels = order_system_labels(ctx, Some(&shard.name));
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": name,
            "namespace": ctx.namespace,
            "labels": labels_value(&labels),
        },
        "spec": {
            "type": "ClusterIP",
            "selector": labels_value(&labels),
            "ports": [{
                "name": "http",
                "port": ctx.system.spec.port,
                "targetPort": "http",
                "protocol": "TCP",
                "appProtocol": "http",
            }],
        },
    })
}

fn order_system_pdb(ctx: &OrderSystemCtx<'_>, shard: &crate::model::OrderSystemShard) -> Value {
    let name = dns_label(&format!("{}-{}", ctx.system.name, shard.name));
    let labels = order_system_labels(ctx, Some(&shard.name));
    json!({
        "apiVersion": "policy/v1",
        "kind": "PodDisruptionBudget",
        "metadata": {
            "name": name,
            "namespace": ctx.namespace,
            "labels": labels_value(&labels),
        },
        "spec": {
            "minAvailable": ctx.system.spec.min_replicas().saturating_sub(1),
            "selector": { "matchLabels": labels_value(&labels) },
        },
    })
}

fn order_system_hpa(ctx: &OrderSystemCtx<'_>, shard: &crate::model::OrderSystemShard) -> Value {
    let spec = &ctx.system.spec;
    let name = dns_label(&format!("{}-{}", ctx.system.name, shard.name));
    let labels = order_system_labels(ctx, Some(&shard.name));
    json!({
        "apiVersion": "autoscaling/v2",
        "kind": "HorizontalPodAutoscaler",
        "metadata": {
            "name": name,
            "namespace": ctx.namespace,
            "labels": labels_value(&labels),
        },
        "spec": {
            "scaleTargetRef": {
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "name": name,
            },
            "minReplicas": spec.min_replicas(),
            "maxReplicas": spec.max_replicas(),
            "metrics": [{
                "type": "Resource",
                "resource": {
                    "name": "cpu",
                    "target": {
                        "type": "Utilization",
                        "averageUtilization": spec.autoscaling.target_cpu_utilization,
                    },
                },
            }],
            "behavior": {
                "scaleUp": {
                    "stabilizationWindowSeconds": 30,
                    "policies": [{ "type": "Percent", "value": 100, "periodSeconds": 30 }],
                },
                "scaleDown": {
                    "stabilizationWindowSeconds": 300,
                    "policies": [{ "type": "Percent", "value": 50, "periodSeconds": 60 }],
                },
            },
        },
    })
}

fn order_system_ingress(
    ctx: &OrderSystemCtx<'_>,
    shard: &crate::model::OrderSystemShard,
) -> Option<Value> {
    let base = ctx.system.spec.ingress.trim().trim_end_matches('.');
    if base.is_empty() {
        return None;
    }
    let name = dns_label(&format!("{}-{}", ctx.system.name, shard.name));
    let labels = order_system_labels(ctx, Some(&shard.name));
    let host = format!("{}.{}", shard.name, base);
    let mut spec = json!({
        "rules": [{
            "host": host,
            "http": { "paths": [{
                "path": "/",
                "pathType": "Prefix",
                "backend": { "service": { "name": name, "port": { "name": "http" } } },
            }] },
        }],
    });
    if !ctx.system.spec.tls_secret.trim().is_empty() {
        spec["tls"] = json!([{ "hosts": [host], "secretName": ctx.system.spec.tls_secret }]);
    }
    Some(json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "Ingress",
        "metadata": {
            "name": name,
            "namespace": ctx.namespace,
            "labels": labels_value(&labels),
        },
        "spec": spec,
    }))
}

/// Render one `OrderSystem` into one ConfigMap and a Deployment/Service/PDB/Ingress
/// set per user-ID shard. Every shard receives the same target list.
pub fn render_order_system(
    catalog: &Catalog,
    name: &str,
    namespace: Option<&str>,
) -> Result<Vec<Value>, ApiError> {
    let system = catalog.order_systems.get(name).ok_or_else(|| {
        ApiError::not_found(
            "order_system_not_found",
            format!("unknown order system {name}"),
        )
    })?;
    let cell = catalog.cells.get(&system.spec.cell).ok_or_else(|| {
        market_data_error(
            "order_system_cell",
            name,
            format!("cell {} is not registered", system.spec.cell),
        )
    })?;
    if system.spec.image.trim().is_empty() {
        return Err(market_data_error(
            "order_system_image",
            name,
            "spec.image is required",
        ));
    }
    if system.spec.port == 0 {
        return Err(market_data_error(
            "order_system_port",
            name,
            "spec.port must be greater than zero",
        ));
    }
    if system.spec.shards.is_empty() {
        return Err(market_data_error(
            "order_system_shards",
            name,
            "declare at least one user-ID shard",
        ));
    }
    let mut targets = Vec::new();
    for target in &system.spec.targets {
        let symbol = catalog.symbols.get(&target.symbol).ok_or_else(|| {
            market_data_error(
                "order_system_symbol",
                name,
                format!("unknown symbol {}", target.symbol),
            )
        })?;
        let deployment = catalog
            .deployments
            .values()
            .find(|deployment| deployment.spec.symbol_ref == target.symbol)
            .ok_or_else(|| {
                market_data_error(
                    "order_system_deployment",
                    name,
                    format!("symbol {} has no engine deployment", target.symbol),
                )
            })?;
        let shard = catalog
            .shards
            .get(&deployment.spec.shard_ref)
            .ok_or_else(|| {
                market_data_error(
                    "order_system_shard",
                    name,
                    format!(
                        "deployment {} references unknown shard {}",
                        deployment.name, deployment.spec.shard_ref
                    ),
                )
            })?;
        let ns = if deployment.namespace.is_empty() {
            system.namespace.clone()
        } else {
            deployment.namespace.clone()
        };
        let engine = format!(
            "http://{}-0.{}-headless.{}.svc.cluster.local:{}",
            shard.name, shard.name, ns, deployment.spec.placement.http_port
        );
        // Enumerate every Raft member so the order-system client can discover
        // the current leader from `GET /metrics` if its target pod is down.
        // Pod ordinal maps to raft id `raft_id_offset + ordinal` (the engine is
        // started with `--id $(POD_ORDINAL) --id-offset <raft_id_offset>`).
        let engine_members = (0..shard.spec.replicas)
            .map(|ordinal| {
                (
                    deployment.spec.raft_id_offset + ordinal as u64,
                    format!(
                        "{}-{}.{}-headless.{}.svc.cluster.local:{}",
                        shard.name, ordinal, shard.name, ns, deployment.spec.placement.http_port
                    ),
                )
            })
            .collect();
        targets.push(OrderSystemTargetResolved {
            id: symbol.name.clone(),
            label: if target.label.trim().is_empty() {
                symbol.name.clone()
            } else {
                target.label.clone()
            },
            engine,
            engine_id: deployment.spec.raft_id_offset,
            product: target.product,
            engine_members,
        });
    }
    if targets.is_empty() {
        return Err(market_data_error(
            "order_system_targets",
            name,
            "declare at least one engine/symbol target",
        ));
    }
    let namespace = namespace
        .map(str::to_string)
        .filter(|namespace| !namespace.is_empty())
        .or_else(|| {
            if system.namespace.is_empty() {
                Some(cell.namespace.clone())
            } else {
                Some(system.namespace.clone())
            }
        })
        .unwrap_or_default();
    let ctx = OrderSystemCtx {
        system,
        cell,
        namespace,
        targets,
    };
    let config = order_system_config(&ctx);
    let mut objects = vec![order_system_config_map(&ctx, &config)];
    for shard in &system.spec.shards {
        objects.push(order_system_deployment(&ctx, shard, &config));
        objects.push(order_system_service(&ctx, shard));
        objects.push(order_system_pdb(&ctx, shard));
        if !system.spec.paused {
            objects.push(order_system_hpa(&ctx, shard));
        }
        if let Some(ingress) = order_system_ingress(&ctx, shard) {
            objects.push(ingress);
        }
    }
    Ok(objects)
}

/// Whether a shard is safe to roll one pod at a time for the target image.
///
/// Same-version restarts are always safe. A different wire/snapshot version is stop-the-world
/// unless the target release declares mixed-version support.
pub fn rolling_upgrade_safe(shard: &SymbolShard, target_release: &crate::model::Release) -> bool {
    shard.spec.release.can_roll_to(target_release)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::demo_catalog;

    fn object<'a>(objects: &'a [Value], kind: &str) -> &'a Value {
        objects
            .iter()
            .find(|o| o["kind"] == kind)
            .unwrap_or_else(|| panic!("missing {kind}"))
    }

    #[test]
    fn statefulset_template_records_release_annotations() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap();
        let annotations =
            &object(&objects, "StatefulSet")["spec"]["template"]["metadata"]["annotations"];
        assert_eq!(annotations["matching.io/release-wire"], "7");
        assert_eq!(annotations["matching.io/release-snapshot"], "11");
        assert_eq!(annotations["matching.io/release-mixed-safe"], "false");
    }

    #[test]
    fn reserved_shard_renders_without_a_statefulset() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "fx-eu-02", &RenderOptions::default()).unwrap();
        let kinds: Vec<&str> = objects
            .iter()
            .map(|object| object["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec!["ConfigMap", "PodDisruptionBudget", "NetworkPolicy"]
        );
        let config = object(&objects, "ConfigMap");
        let slots: Value =
            serde_json::from_str(config["data"]["slots.json"].as_str().unwrap()).unwrap();
        assert!(slots["processes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn supervisor_shard_keeps_the_pod_template_when_a_process_is_added() {
        let mut catalog = demo_catalog();
        {
            let shard = catalog.shards.get_mut("fx-eu-02").expect("reserved shard");
            shard.spec.packing.mode = PackingMode::Supervisor;
        }
        let before = render_shard(&catalog, "fx-eu-02", &RenderOptions::default()).unwrap();
        let before_sts = object(&before, "StatefulSet");
        let before_template = before_sts["spec"]["template"].clone();
        let before_processes: Value = serde_json::from_str(
            object(&before, "ConfigMap")["data"]["processes.json"]
                .as_str()
                .expect("supervisor process list"),
        )
        .unwrap();
        assert!(before_processes["processes"].as_array().unwrap().is_empty());
        assert_eq!(
            before_sts["spec"]["template"]["spec"]["containers"][0]["name"],
            "supervisor"
        );
        let args = before_sts["spec"]["template"]["spec"]["containers"][0]["args"]
            .as_array()
            .unwrap();
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--max-processes" && pair[1] == "4")
        );
        assert_eq!(
            before_sts["spec"]["template"]["spec"]["containers"][0]["resources"]["requests"]["cpu"],
            "250m"
        );

        let mut symbol = catalog.symbols.get("eurusd").expect("demo symbol").clone();
        symbol.name = "sup1".to_string();
        symbol.spec.symbol_id = 9;
        catalog.symbols.insert(symbol.name.clone(), symbol);
        let mut deployment = catalog
            .deployments
            .get("eurusd-g1")
            .expect("demo deployment")
            .clone();
        deployment.name = "sup1-g1".to_string();
        deployment.spec.symbol_ref = "sup1".to_string();
        deployment.spec.shard_ref = "fx-eu-02".to_string();
        deployment.spec.placement = crate::model::Placement {
            slot: 0,
            http_port: 21041,
        };
        catalog
            .deployments
            .insert(deployment.name.clone(), deployment);
        let mut route = catalog.routes.get("eurusd").expect("demo route").clone();
        route.name = "sup1".to_string();
        route.spec.symbol_ref = "sup1".to_string();
        route.spec.active_deployment_ref = "sup1-g1".to_string();
        catalog.routes.insert(route.name.clone(), route);

        let after = render_shard(&catalog, "fx-eu-02", &RenderOptions::default()).unwrap();
        let after_sts = object(&after, "StatefulSet");
        assert_eq!(
            before_template, after_sts["spec"]["template"],
            "adding a process must not change the supervisor pod template"
        );
        let after_processes: Value = serde_json::from_str(
            object(&after, "ConfigMap")["data"]["processes.json"]
                .as_str()
                .expect("supervisor process list"),
        )
        .unwrap();
        assert_eq!(after_processes["processes"].as_array().unwrap().len(), 1);
        assert_eq!(after_processes["processes"][0]["name"], "sup1");
    }

    #[test]
    fn supervisor_render_passes_isolation_and_log_flags() {
        let mut catalog = demo_catalog();
        {
            let shard = catalog.shards.get_mut("fx-eu-02").expect("reserved shard");
            shard.spec.packing.mode = PackingMode::Supervisor;
            shard.spec.packing.cgroup_root = Some("/sys/fs/cgroup/match".to_string());
            shard.spec.packing.child_memory_max = Some("268435456".to_string());
            shard.spec.packing.child_cpu_max = Some("25000 100000".to_string());
            shard.spec.packing.log_dir = Some("/data/logs".to_string());
            shard.spec.packing.log_max_bytes = 1024;
            shard.spec.packing.log_max_files = 3;
        }
        let objects = render_shard(&catalog, "fx-eu-02", &RenderOptions::default()).unwrap();
        let stateful_set = object(&objects, "StatefulSet");
        let args = stateful_set["spec"]["template"]["spec"]["containers"][0]["args"]
            .as_array()
            .unwrap();
        for pair in [
            ["--cgroup-root", "/sys/fs/cgroup/match"],
            ["--child-memory-max", "268435456"],
            ["--child-cpu-max", "25000 100000"],
            ["--log-dir", "/data/logs"],
            ["--log-max-bytes", "1024"],
            ["--log-max-files", "3"],
        ] {
            assert!(
                args.windows(2)
                    .any(|window| window[0] == pair[0] && window[1] == pair[1]),
                "missing {pair:?} in {args:?}"
            );
        }
        assert!(
            stateful_set["spec"]["volumeClaimTemplates"][0]["metadata"]
                .get("labels")
                .is_none(),
            "PVC labels are applied by the operator, not the immutable StatefulSet template"
        );
    }

    #[test]
    fn migration_shadow_renders_with_a_distinct_raft_id_range() {
        let mut catalog = demo_catalog();
        let mut shadow = catalog.deployments["eurusd-g1"].clone();
        shadow.name = "eurusd-shadow".to_string();
        shadow.spec.shard_ref = "fx-eu-02".to_string();
        shadow.spec.placement = crate::model::Placement {
            slot: 0,
            http_port: 21041,
        };
        shadow.spec.raft_id_offset = 4;
        shadow.spec.desired_state = DeploymentState::Shadow;
        catalog.deployments.insert(shadow.name.clone(), shadow);

        let objects = render_shard(&catalog, "fx-eu-02", &RenderOptions::default()).unwrap();
        let stateful_set = object(&objects, "StatefulSet");
        let args = stateful_set["spec"]["template"]["spec"]["containers"][0]["args"]
            .as_array()
            .unwrap();
        assert!(
            args.windows(2)
                .any(|pair| { pair[0] == "--id-offset" && pair[1] == "4" })
        );
        let processes: Value = serde_json::from_str(
            object(&objects, "ConfigMap")["data"]["processes.json"]
                .as_str()
                .expect("supervisor process list"),
        )
        .unwrap();
        assert_eq!(processes["processes"][0]["raftIdOffset"], 4);
    }

    #[test]
    fn packed_shard_renders_the_expected_objects() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap();
        let kinds: Vec<&str> = objects
            .iter()
            .map(|o| o["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec![
                "ConfigMap",
                "Service",
                "Service",
                "PodDisruptionBudget",
                "NetworkPolicy",
                "StatefulSet",
            ]
        );
        let sts = object(&objects, "StatefulSet");
        let containers = sts["spec"]["template"]["spec"]["containers"]
            .as_array()
            .unwrap();
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0]["name"], "eurusd");
        assert_eq!(containers[1]["name"], "gbpusd");
        assert_eq!(
            containers[0]["args"][2], "--id-offset",
            "migration targets use a distinct Raft ID range"
        );
        assert_eq!(containers[0]["args"][3], "0");
        assert_eq!(
            containers[0]["args"][5], "$(POD_FQDN):21001",
            "http address uses the stable pod DNS name"
        );
        assert_eq!(containers[1]["args"][5], "$(POD_FQDN):21002");
        assert_eq!(
            sts["spec"]["template"]["spec"]["automountServiceAccountToken"],
            false
        );
        assert!(
            sts["spec"]["template"]["spec"]
                .get("serviceAccountName")
                .is_none()
        );
        assert_eq!(
            sts["spec"]["template"]["spec"]["securityContext"]["runAsNonRoot"],
            true
        );
        assert_eq!(
            sts["spec"]["template"]["spec"]["securityContext"]["runAsUser"],
            10001
        );
        assert_eq!(sts["spec"]["template"]["spec"]["volumes"][0]["name"], "tmp");
        assert_eq!(sts["spec"]["updateStrategy"]["type"], "OnDelete");
        assert_eq!(sts["spec"]["podManagementPolicy"], "Parallel");
        assert_eq!(
            sts["spec"]["template"]["spec"]["affinity"]["podAntiAffinity"]
                ["requiredDuringSchedulingIgnoredDuringExecution"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            sts["spec"]["template"]["spec"]["topologySpreadConstraints"][0]["whenUnsatisfiable"]
                == "ScheduleAnyway"
        );
        assert_eq!(containers[0]["ports"][0]["containerPort"], 21001);
        assert_eq!(containers[0]["ports"][1]["containerPort"], 22001);
        assert_eq!(
            containers[0]["env"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == "RAFT_WAL_FSYNC_MS")
                .unwrap()["value"],
            "0"
        );
    }

    #[test]
    fn dedicated_shard_gets_a_leader_service() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "me-btcusdt", &RenderOptions::default()).unwrap();
        let services: Vec<&Value> = objects.iter().filter(|o| o["kind"] == "Service").collect();
        assert_eq!(services.len(), 3);
        assert!(
            services
                .iter()
                .any(|s| s["metadata"]["name"] == "me-btcusdt-leader")
        );
        let sts = object(&objects, "StatefulSet");
        assert_eq!(sts["spec"]["replicas"], 4);
    }

    #[test]
    fn headless_service_publishes_peers_before_ready() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap();
        let headless = objects
            .iter()
            .find(|o| o["metadata"]["name"] == "fx-eu-01-headless")
            .unwrap();
        assert_eq!(headless["spec"]["clusterIP"], "None");
        assert_eq!(headless["spec"]["publishNotReadyAddresses"], true);
        assert_eq!(headless["spec"]["ports"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn service_ports_only_use_kubernetes_fields() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap();
        for service in objects.iter().filter(|object| object["kind"] == "Service") {
            for port in service["spec"]["ports"].as_array().unwrap() {
                for key in port.as_object().unwrap().keys() {
                    assert!(
                        matches!(
                            key.as_str(),
                            "name"
                                | "port"
                                | "targetPort"
                                | "protocol"
                                | "appProtocol"
                                | "nodePort"
                        ),
                        "unexpected ServicePort field {key}"
                    );
                }
            }
        }
    }

    #[test]
    fn pdb_tolerates_one_voter() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap();
        assert_eq!(
            object(&objects, "PodDisruptionBudget")["spec"]["minAvailable"],
            3
        );
    }

    #[test]
    fn network_policy_uses_the_documented_ranges() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap();
        let policy = object(&objects, "NetworkPolicy");
        assert_eq!(policy["spec"]["ingress"][0]["ports"][0]["port"], 22001);
        assert_eq!(policy["spec"]["ingress"][0]["ports"][0]["endPort"], 22020);
        assert_eq!(policy["spec"]["ingress"][1]["ports"][0]["port"], 21001);
        assert_eq!(policy["spec"]["egress"][1]["ports"][0]["port"], 443);
        assert_eq!(
            policy["spec"]["egress"][2]["to"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            "kube-system"
        );
        // The engine console's cluster view probes peers over HTTP.
        assert_eq!(policy["spec"]["ingress"][2]["ports"][0]["port"], 21001);
        assert_eq!(policy["spec"]["ingress"][2]["ports"][0]["endPort"], 21020);
        assert_eq!(policy["spec"]["egress"][3]["ports"][0]["port"], 21001);
        assert_eq!(policy["spec"]["egress"][3]["ports"][0]["endPort"], 21020);
        // Migration peers in the same cell may reach each other across shards.
        let cluster_id = catalog.cells["eu-1"].spec.cluster_id.as_str();
        assert_eq!(
            policy["spec"]["ingress"][3]["from"][0]["podSelector"]["matchLabels"]
                ["matching.io/cluster-id"]
                .as_str(),
            Some(cluster_id)
        );
        assert!(policy["spec"]["ingress"][3].get("ports").is_none());
        assert!(policy["spec"]["egress"][4].get("ports").is_none());
    }

    #[test]
    fn local_profile_relaxes_scheduling_and_storage() {
        let catalog = demo_catalog();
        let opts = RenderOptions {
            profile: Profile::Local,
            storage_class: Some("local-path".to_string()),
            ..Default::default()
        };
        let objects = render_shard(&catalog, "fx-eu-01", &opts).unwrap();
        let sts = object(&objects, "StatefulSet");
        let pod = &sts["spec"]["template"]["spec"];
        assert!(
            pod["affinity"]["podAntiAffinity"]
                .get("requiredDuringSchedulingIgnoredDuringExecution")
                .is_none()
        );
        assert!(pod.get("topologySpreadConstraints").is_none());
        assert_eq!(
            sts["spec"]["volumeClaimTemplates"][0]["spec"]["storageClassName"],
            "local-path"
        );
    }

    #[test]
    fn render_refuses_invalid_placement() {
        let mut catalog = demo_catalog();
        let deployment = catalog
            .deployments
            .get_mut("gbpusd-g1")
            .expect("demo deployment");
        deployment.spec.placement.http_port = 21009;
        let error = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap_err();
        assert_eq!(error.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            error.issues.iter().any(|i| i.code == "port_convention"),
            "{:?}",
            error.issues
        );
    }

    #[test]
    fn rolling_upgrade_safety_follows_wire_versions() {
        let catalog = demo_catalog();
        let shard = catalog.shards.get("fx-eu-01").unwrap();
        let same = crate::model::Release::default();
        assert!(rolling_upgrade_safe(shard, &same));
        let mut next = same.clone();
        next.wire_version += 1;
        assert!(!rolling_upgrade_safe(shard, &next));
        next.mixed_version_safe = true;
        assert!(rolling_upgrade_safe(shard, &next));
    }

    #[test]
    fn list_wrapper_is_kubectl_friendly() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap();
        let list = as_list(objects);
        assert_eq!(list["kind"], "List");
        assert_eq!(list["items"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn config_map_is_a_derived_cache() {
        let catalog = demo_catalog();
        let objects = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap();
        let config = object(&objects, "ConfigMap");
        let slots: Value =
            serde_json::from_str(config["data"]["slots.json"].as_str().unwrap()).unwrap();
        assert_eq!(slots["processes"][0]["name"], "eurusd");
        assert_eq!(slots["processes"][0]["raftPort"], 22001);
        assert_eq!(slots["processes"][1]["deploymentRef"], "gbpusd-g1");
    }

    fn demo_market_data(catalog: &mut Catalog, name: &str) -> crate::model::MarketDataService {
        use crate::model::{
            MarketDataFeedGroup, MarketDataService, MarketDataServiceSpec, MarketDataTransport,
            MarketDataWebSocket,
        };
        let service = MarketDataService {
            name: name.to_string(),
            namespace: "match-eu-1".to_string(),
            spec: MarketDataServiceSpec {
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
                websocket: MarketDataWebSocket {
                    port: 24800,
                    max_connections: 4096,
                    ..Default::default()
                },
                autoscaling: Default::default(),
                resources: None,
                paused: false,
            },
            status: Default::default(),
        };
        catalog
            .market_data_services
            .insert(name.to_string(), service.clone());
        service
    }

    fn demo_nats_market_data(catalog: &mut Catalog, name: &str) -> crate::model::MarketDataService {
        use crate::model::{
            MarketDataFeedGroup, MarketDataService, MarketDataServiceSpec, MarketDataTransport,
            MarketDataWebSocket,
        };
        let service = MarketDataService {
            name: name.to_string(),
            namespace: "match-eu-1".to_string(),
            spec: MarketDataServiceSpec {
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
                    mode: MarketDataTransportMode::Nats,
                    nats_url: Some("nats://nats.match-eu-1.svc.cluster.local:4222".to_string()),
                    ..Default::default()
                },
                websocket: MarketDataWebSocket {
                    port: 24800,
                    max_connections: 4096,
                    ..Default::default()
                },
                autoscaling: Default::default(),
                resources: None,
                paused: false,
            },
            status: Default::default(),
        };
        catalog
            .market_data_services
            .insert(name.to_string(), service.clone());
        service
    }

    fn demo_nats_groups_service(
        catalog: &mut Catalog,
        name: &str,
        groups: &[(&str, &str)],
    ) -> crate::model::MarketDataService {
        use crate::model::MarketDataFeedGroup;
        let mut service = demo_nats_market_data(catalog, name);
        service.spec.feed_groups = groups
            .iter()
            .map(|(group, symbol)| MarketDataFeedGroup {
                name: (*group).to_string(),
                symbols: vec![(*symbol).to_string()],
                stream_base: 2001,
                channel: None,
            })
            .collect();
        catalog
            .market_data_services
            .insert(name.to_string(), service.clone());
        service
    }

    fn deployment_args(objects: &[Value]) -> Vec<String> {
        object(objects, "Deployment")["spec"]["template"]["spec"]["containers"][0]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn stopped_symbol_leaves_the_market_data_feed() {
        let mut catalog = demo_catalog();
        demo_nats_groups_service(
            &mut catalog,
            "fx-marketdata",
            &[("fx-a", "gbpusd"), ("eurusd", "eurusd")],
        );
        catalog
            .deployments
            .get_mut("eurusd-g1")
            .unwrap()
            .spec
            .desired_state = DeploymentState::Disabled;

        let objects = render_market_data_service(&catalog, "fx-marketdata", None).unwrap();
        let args = deployment_args(&objects);
        assert!(args.iter().any(|arg| arg == "gbpusd:2:GBP:USD"), "{args:?}");
        assert!(
            !args.iter().any(|arg| arg.starts_with("eurusd:")),
            "{args:?}"
        );
        assert!(args.iter().any(|arg| arg == "md.eu-1.fx-a"), "{args:?}");
        assert!(!args.iter().any(|arg| arg == "md.eu-1.eurusd"), "{args:?}");

        let config = object(&objects, "ConfigMap");
        let config: Value =
            serde_json::from_str(config["data"]["marketdata.json"].as_str().unwrap()).unwrap();
        let eurusd = config["feedGroups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|group| group["name"] == "eurusd")
            .unwrap();
        assert!(
            eurusd["symbols"].as_array().unwrap().is_empty(),
            "{eurusd:?}"
        );
    }

    #[test]
    fn all_stopped_symbols_scale_market_data_to_zero() {
        let mut catalog = demo_catalog();
        demo_nats_groups_service(
            &mut catalog,
            "fx-marketdata",
            &[("fx-a", "gbpusd"), ("eurusd", "eurusd")],
        );
        for deployment in catalog.deployments.values_mut() {
            deployment.spec.desired_state = DeploymentState::Disabled;
        }

        let objects = render_market_data_service(&catalog, "fx-marketdata", None).unwrap();
        let deployment = object(&objects, "Deployment");
        assert_eq!(deployment["spec"]["replicas"], 0);
        let args = deployment_args(&objects);
        assert!(!args.iter().any(|arg| arg == "--symbol"), "{args:?}");
        assert!(
            !objects
                .iter()
                .any(|object| object["kind"] == "HorizontalPodAutoscaler"),
            "an HPA would scale a zero-symbol service back up"
        );
    }

    #[test]
    fn nats_market_data_service_renders_a_nats_source() {
        let mut catalog = demo_catalog();
        demo_nats_market_data(&mut catalog, "btc-marketdata");
        let objects =
            render_market_data_service(&catalog, "btc-marketdata", Some("match-eu-1")).unwrap();
        let deployment = object(&objects, "Deployment");
        let args: Vec<&str> = deployment["spec"]["template"]["spec"]["containers"][0]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        assert!(args.contains(&"--nats-url"), "{args:?}");
        assert!(
            args.contains(&"nats://nats.match-eu-1.svc.cluster.local:4222"),
            "{args:?}"
        );
        assert!(args.contains(&"--nats-subject-prefix"), "{args:?}");
        assert!(args.contains(&"md.eu-1.btc"), "{args:?}");
        assert!(args.contains(&"--nats-stream-base"), "{args:?}");
        assert!(!args.contains(&"--aeron-channel"), "{args:?}");
        assert!(!args.contains(&"--aeron-embedded-driver"), "{args:?}");
        let policy = object(&objects, "NetworkPolicy");
        assert!(
            policy["spec"]["egress"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule["ports"].as_array().is_some_and(|ports| ports
                    .iter()
                    .any(|port| { port["protocol"] == "TCP" && port["port"] == 4222 }))),
            "{policy:?}"
        );
        assert!(
            !policy["spec"]["egress"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule["ports"].as_array().is_some_and(|ports| ports
                    .iter()
                    .any(|port| port["protocol"] == "UDP" && port["port"] == 24001))),
            "{policy:?}"
        );
    }

    #[test]
    fn nats_services_subscribe_to_the_canonical_engine_subject() {
        use crate::model::{
            MarketDataFeedGroup, MarketDataService, MarketDataServiceSpec, MarketDataTransport,
            MarketDataWebSocket,
        };
        let mut catalog = demo_catalog();
        demo_nats_market_data(&mut catalog, "btc-marketdata");
        // A larger global service uses a different local group name. It must
        // still subscribe to the shard-local service's canonical subject.
        catalog.market_data_services.insert(
            "global-marketdata".to_string(),
            MarketDataService {
                name: "global-marketdata".to_string(),
                namespace: "match-eu-1".to_string(),
                spec: MarketDataServiceSpec {
                    cell: "eu-1".to_string(),
                    symbols: Vec::new(),
                    feed_groups: vec![MarketDataFeedGroup {
                        name: "global-btc".to_string(),
                        symbols: vec!["btcusdt".to_string()],
                        stream_base: 2001,
                        channel: None,
                    }],
                    image: "registry.example.com/matchengine-marketdata@sha256:abc".to_string(),
                    release: Default::default(),
                    replicas: 1,
                    transport: MarketDataTransport {
                        mode: MarketDataTransportMode::Nats,
                        nats_url: Some("nats://nats.match-eu-1.svc.cluster.local:4222".to_string()),
                        ..Default::default()
                    },
                    websocket: MarketDataWebSocket {
                        port: 24801,
                        ..Default::default()
                    },
                    autoscaling: Default::default(),
                    resources: None,
                    paused: false,
                },
                status: Default::default(),
            },
        );
        let objects =
            render_market_data_service(&catalog, "global-marketdata", Some("match-eu-1")).unwrap();
        let args: Vec<&str> = objects
            .iter()
            .find(|object| object["kind"] == "Deployment")
            .unwrap()["spec"]["template"]["spec"]["containers"][0]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        assert!(args.contains(&"md.eu-1.btc"), "{args:?}");
        assert!(!args.contains(&"md.eu-1.global-btc"), "{args:?}");
    }

    #[test]
    fn engine_container_publishes_to_nats() {
        let mut catalog = demo_catalog();
        demo_nats_market_data(&mut catalog, "btc-marketdata");
        let objects = render_shard(
            &catalog,
            "me-btcusdt",
            &RenderOptions {
                namespace: Some("match-eu-1".to_string()),
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let stateful_set = object(&objects, "StatefulSet");
        let pod = &stateful_set["spec"]["template"]["spec"];
        let container = &pod["containers"][0];
        let args: Vec<&str> = container["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        assert!(args.contains(&"--market-data-nats-url"), "{args:?}");
        assert!(
            args.contains(&"nats://nats.match-eu-1.svc.cluster.local:4222"),
            "{args:?}"
        );
        assert!(
            args.contains(&"--market-data-nats-subject-prefix"),
            "{args:?}"
        );
        assert!(args.contains(&"md.eu-1.btc"), "{args:?}");
        assert!(args.contains(&"--market-data-nats-stream-base"), "{args:?}");
        assert!(!args.contains(&"--market-data-aeron-channel"), "{args:?}");
        assert!(
            !pod["volumes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|volume| volume["name"] == "aeron"),
            "{pod:?}"
        );
        let policy = object(&objects, "NetworkPolicy");
        assert!(
            policy["spec"]["egress"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule["ports"].as_array().is_some_and(|ports| ports
                    .iter()
                    .any(|port| { port["protocol"] == "TCP" && port["port"] == 4222 }))),
            "{policy:?}"
        );
    }

    #[test]
    fn order_system_renders_hpa_and_rolling_shards() {
        use crate::model::{
            OrderSystem, OrderSystemAutoscaling, OrderSystemShard, OrderSystemSpec,
            OrderSystemTarget,
        };
        let mut catalog = demo_catalog();
        let system = OrderSystem {
            name: "orders".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: OrderSystemSpec {
                cell: "eu-1".to_string(),
                image: "matchengine-order-system:arm64-v2".to_string(),
                replicas: 1,
                autoscaling: OrderSystemAutoscaling {
                    min_replicas: 1,
                    max_replicas: 2,
                    target_cpu_utilization: 70,
                },
                port: 24300,
                targets: vec![
                    OrderSystemTarget {
                        symbol: "gbpusd".to_string(),
                        label: "GBP/USD".to_string(),
                        product: Default::default(),
                    },
                    OrderSystemTarget {
                        symbol: "btcusdt".to_string(),
                        label: "BTC/USDT".to_string(),
                        product: Default::default(),
                    },
                ],
                shards: vec![
                    OrderSystemShard {
                        name: "users-0".to_string(),
                        user_id_min: 0,
                        user_id_max: 499,
                    },
                    OrderSystemShard {
                        name: "users-1".to_string(),
                        user_id_min: 500,
                        user_id_max: 999,
                    },
                ],
                ingress: "orders.match-eu-1.example".to_string(),
                tls_secret: String::new(),
                resources: None,
                paused: false,
            },
            status: Default::default(),
        };
        catalog.order_systems.insert(system.name.clone(), system);
        let objects = render_order_system(&catalog, "orders", None).unwrap();
        let hpas: Vec<&Value> = objects
            .iter()
            .filter(|object| object["kind"] == "HorizontalPodAutoscaler")
            .collect();
        assert_eq!(2, hpas.len());
        assert_eq!(1, hpas[0]["spec"]["minReplicas"]);
        assert_eq!(2, hpas[0]["spec"]["maxReplicas"]);
        assert_eq!(
            70,
            hpas[0]["spec"]["metrics"][0]["resource"]["target"]["averageUtilization"]
        );
        let deployments: Vec<&Value> = objects
            .iter()
            .filter(|object| object["kind"] == "Deployment")
            .collect();
        assert_eq!(2, deployments.len());
        // The HPA owns the replica count while the system is running.
        assert!(deployments[0]["spec"].get("replicas").is_none());
        assert_eq!(
            0,
            deployments[0]["spec"]["strategy"]["rollingUpdate"]["maxUnavailable"]
        );
        assert_eq!(
            1,
            deployments[0]["spec"]["strategy"]["rollingUpdate"]["maxSurge"]
        );
        assert_eq!(
            1,
            deployments[0]["spec"]["template"]["spec"]["containers"][0]["args"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|arg| *arg == "--user-id-min")
                .count()
        );

        // The target dials pod 0, whose node ID is the deployment's Raft ID
        // offset (the pod ordinal is added by `--id $(POD_ORDINAL)`).
        let config_map = objects
            .iter()
            .find(|object| object["kind"] == "ConfigMap")
            .expect("order-system ConfigMap");
        let targets: Value = serde_json::from_str(
            config_map["data"]["targets.json"]
                .as_str()
                .expect("targets.json"),
        )
        .expect("targets JSON");
        let targets = targets["targets"].as_array().expect("targets array");
        assert_eq!(2, targets.len());
        for target in targets {
            assert_eq!(0, target["engineId"]);
            assert!(
                target["engine"]
                    .as_str()
                    .is_some_and(|engine| engine.contains("-0.")),
                "target dials pod 0: {target}"
            );
            let members = target["engineMembers"]
                .as_array()
                .expect("engineMembers array");
            assert_eq!(4, members.len());
            assert_eq!(0, members[0]["nodeId"]);
            assert!(
                members[0]["addr"]
                    .as_str()
                    .is_some_and(|addr| addr.contains("-0."))
            );
            assert_eq!(3, members[3]["nodeId"]);
            assert!(
                members[3]["addr"]
                    .as_str()
                    .is_some_and(|addr| addr.contains("-3."))
            );
        }

        catalog.order_systems.get_mut("orders").unwrap().spec.paused = true;
        let paused = render_order_system(&catalog, "orders", None).unwrap();
        assert!(
            paused
                .iter()
                .all(|object| object["kind"] != "HorizontalPodAutoscaler")
        );
        let deployments: Vec<&Value> = paused
            .iter()
            .filter(|object| object["kind"] == "Deployment")
            .collect();
        assert_eq!(0, deployments[0]["spec"]["replicas"]);
    }

    #[test]
    fn global_market_data_service_uses_per_group_channels() {
        use crate::model::{
            MarketDataFeedGroup, MarketDataService, MarketDataServiceSpec, MarketDataTransport,
            MarketDataWebSocket,
        };
        let mut catalog = demo_catalog();
        let fx_channel = "aeron:udp?endpoint=239.1.2.65:24031|interface={pod_ip}|ttl=1";
        let btc_channel = "aeron:udp?endpoint=239.1.2.51:24001|interface={pod_ip}|ttl=1";
        let service = MarketDataService {
            name: "global-marketdata".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: MarketDataServiceSpec {
                cell: "eu-1".to_string(),
                symbols: Vec::new(),
                feed_groups: vec![
                    MarketDataFeedGroup {
                        name: "fx-eu-01-eurusd".to_string(),
                        symbols: vec!["eurusd".to_string()],
                        stream_base: 2001,
                        channel: Some(fx_channel.to_string()),
                    },
                    MarketDataFeedGroup {
                        name: "fx-eu-01-gbpusd".to_string(),
                        symbols: vec!["gbpusd".to_string()],
                        stream_base: 2011,
                        channel: Some(fx_channel.to_string()),
                    },
                    MarketDataFeedGroup {
                        name: "me-btcusdt".to_string(),
                        symbols: vec!["btcusdt".to_string()],
                        stream_base: 2001,
                        channel: Some(btc_channel.to_string()),
                    },
                ],
                image: "registry.example.com/matchengine-marketdata@sha256:abc".to_string(),
                release: Default::default(),
                replicas: 2,
                transport: MarketDataTransport::default(),
                websocket: MarketDataWebSocket {
                    port: 24804,
                    ingress: "ws-fx-eu-02.match-eu-1.example".to_string(),
                    ..Default::default()
                },
                autoscaling: Default::default(),
                resources: None,
                paused: false,
            },
            status: Default::default(),
        };
        catalog
            .market_data_services
            .insert(service.name.clone(), service);
        let objects = render_market_data_service(&catalog, "global-marketdata", None).unwrap();
        let kinds: Vec<&str> = objects
            .iter()
            .map(|object| object["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"Ingress"), "{kinds:?}");
        let deployment = object(&objects, "Deployment");
        let container = &deployment["spec"]["template"]["spec"]["containers"][0];
        let args: Vec<&str> = container["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        let channels: Vec<&str> = args
            .iter()
            .enumerate()
            .filter(|(_, arg)| **arg == "--aeron-channel")
            .map(|(index, _)| args[index + 1])
            .collect();
        assert_eq!(
            vec![
                "aeron:udp?endpoint=239.1.2.65:24031|ttl=1",
                "aeron:udp?endpoint=239.1.2.65:24031|ttl=1",
                "aeron:udp?endpoint=239.1.2.51:24001|ttl=1"
            ],
            channels
        );
        assert!(!args.contains(&"--fast-only"));
        let config = object(&objects, "ConfigMap");
        let config: Value =
            serde_json::from_str(config["data"]["marketdata.json"].as_str().unwrap()).unwrap();
        assert_eq!(
            config["feedGroups"][0]["channel"],
            "aeron:udp?endpoint=239.1.2.65:24031|ttl=1"
        );
        assert_eq!(
            config["feedGroups"][2]["channel"],
            "aeron:udp?endpoint=239.1.2.51:24001|ttl=1"
        );
        assert_eq!(
            config["feedGroups"][2]["symbols"][0]["channel"],
            "aeron:udp?endpoint=239.1.2.51:24001|ttl=1"
        );
        let policy = object(&objects, "NetworkPolicy");
        let ports: Vec<u64> = policy["spec"]["egress"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|rule| rule["ports"].as_array())
            .flatten()
            .filter_map(|port| port["port"].as_u64())
            .collect();
        assert!(
            ports.contains(&24031) && ports.contains(&24001),
            "{ports:?}"
        );
        let ingress = object(&objects, "Ingress");
        assert_eq!(
            ingress["spec"]["rules"][0]["host"],
            "ws-fx-eu-02.match-eu-1.example"
        );
        assert_eq!(
            ingress["spec"]["rules"][0]["http"]["paths"][0]["backend"]["service"]["name"],
            "global-marketdata"
        );
    }

    #[test]
    fn fast_only_market_data_service_passes_the_flag() {
        let mut catalog = demo_catalog();
        let mut service = demo_market_data(&mut catalog, "fx-fast");
        service.spec.websocket.fast_only = true;
        service.spec.websocket.ingress = "fast-ws.example".to_string();
        catalog
            .market_data_services
            .insert(service.name.clone(), service);
        let objects = render_market_data_service(&catalog, "fx-fast", None).unwrap();
        let deployment = object(&objects, "Deployment");
        let args: Vec<&str> = deployment["spec"]["template"]["spec"]["containers"][0]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        assert!(args.contains(&"--fast-only"), "{args:?}");
        let config = object(&objects, "ConfigMap");
        let config: Value =
            serde_json::from_str(config["data"]["marketdata.json"].as_str().unwrap()).unwrap();
        assert_eq!(config["websocket"]["fastOnly"], true);
        assert_eq!(
            object(&objects, "Ingress")["spec"]["rules"][0]["host"],
            "fast-ws.example"
        );
    }

    #[test]
    fn market_data_service_renders_the_fanout_objects() {
        let mut catalog = demo_catalog();
        demo_market_data(&mut catalog, "btc-marketdata");
        let objects = render_market_data_service(&catalog, "btc-marketdata", None).unwrap();
        let kinds: Vec<&str> = objects
            .iter()
            .map(|object| object["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "ConfigMap",
                "Deployment",
                "Service",
                "PodDisruptionBudget",
                "NetworkPolicy",
                "HorizontalPodAutoscaler"
            ]
        );
        let deployment = object(&objects, "Deployment");
        assert_eq!(deployment["metadata"]["name"], "btc-marketdata");
        // The HPA owns the replica count while the service is running.
        assert!(deployment["spec"].get("replicas").is_none());
        assert_eq!(
            deployment["spec"]["strategy"]["rollingUpdate"]["maxUnavailable"],
            0
        );
        assert_eq!(
            deployment["spec"]["strategy"]["rollingUpdate"]["maxSurge"],
            1
        );
        let container = &deployment["spec"]["template"]["spec"]["containers"][0];
        let args: Vec<&str> = container["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        assert!(args.contains(&"--symbol"), "{args:?}");
        assert!(args.contains(&"btcusdt:3:BTC:USDT"), "{args:?}");
        assert!(args.contains(&"--aeron-stream-base"), "{args:?}");
        assert!(args.contains(&"2001"), "{args:?}");
        assert!(args.contains(&"--aeron-embedded-driver"), "{args:?}");
        assert!(args.contains(&"--aeron-dir"), "{args:?}");
        assert!(args.contains(&"--fast-max-streams"), "{args:?}");
        assert!(args.contains(&"--terminal-conflation-ms"), "{args:?}");
        assert_eq!(container["ports"][0]["containerPort"], 24800);
        assert_eq!(
            container["lifecycle"]["preStop"]["exec"]["command"][2],
            "sleep 10"
        );
        assert_eq!(container["resources"]["requests"]["cpu"], "100m");
        let service = object(&objects, "Service");
        assert_eq!(service["spec"]["ports"][0]["port"], 24800);
        let pdb = object(&objects, "PodDisruptionBudget");
        assert_eq!(pdb["spec"]["minAvailable"], 0);
        let hpa = object(&objects, "HorizontalPodAutoscaler");
        assert_eq!(hpa["spec"]["minReplicas"], 1);
        assert_eq!(hpa["spec"]["maxReplicas"], 2);
        assert_eq!(
            hpa["spec"]["metrics"][0]["resource"]["target"]["averageUtilization"],
            70
        );
        let policy = object(&objects, "NetworkPolicy");
        assert_eq!(
            policy["spec"]["egress"][1]["ports"][0]["port"], 24001,
            "the multicast egress port comes from the channel"
        );
        assert!(
            policy["spec"]["ingress"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule["ports"].as_array().is_some_and(|ports| ports
                    .iter()
                    .any(|port| port["protocol"] == "UDP" && port["port"] == 24001))),
            "engine multicast ingress is allowed: {policy:?}"
        );
        let config = object(&objects, "ConfigMap");
        let data: Value =
            serde_json::from_str(config["data"]["marketdata.json"].as_str().unwrap()).unwrap();
        assert_eq!(data["feedGroups"][0]["symbols"][0]["symbolId"], 3);
        assert_eq!(
            deployment["spec"]["template"]["metadata"]["annotations"]["matching.io/config-hash"],
            deployment["metadata"]["annotations"]["matching.io/config-hash"]
        );
    }

    #[test]
    fn engine_container_publishes_to_a_market_data_service() {
        let mut catalog = demo_catalog();
        demo_market_data(&mut catalog, "btc-marketdata");
        let objects = render_shard(
            &catalog,
            "me-btcusdt",
            &RenderOptions {
                namespace: Some("match-eu-1".to_string()),
                ..RenderOptions::default()
            },
        )
        .unwrap();
        let stateful_set = object(&objects, "StatefulSet");
        let pod = &stateful_set["spec"]["template"]["spec"];
        let container = &pod["containers"][0];
        let args: Vec<&str> = container["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        assert!(args.contains(&"--market-data-aeron-channel"), "{args:?}");
        assert!(
            args.contains(&"aeron:udp?endpoint=239.1.2.51:24001|interface=$(POD_IP)|ttl=1"),
            "{args:?}"
        );
        assert!(
            args.contains(&"--market-data-aeron-embedded-driver"),
            "{args:?}"
        );
        assert!(
            args.contains(&"--market-data-aeron-stream-base"),
            "{args:?}"
        );
        assert!(args.contains(&"2001"), "{args:?}");
        assert!(args.contains(&"--market-data-aeron-dir"), "{args:?}");
        assert!(args.contains(&"/dev/shm/aeron-btcusdt"), "{args:?}");
        assert!(
            pod["volumes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|volume| volume["name"] == "aeron"),
            "{pod:?}"
        );
        assert!(
            container["volumeMounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|mount| mount["name"] == "aeron" && mount["mountPath"] == "/dev/shm"),
            "{container:?}"
        );
        let policy = object(&objects, "NetworkPolicy");
        assert!(
            policy["spec"]["egress"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule["ports"].as_array().is_some_and(|ports| ports
                    .iter()
                    .any(|port| { port["protocol"] == "UDP" && port["port"] == 24001 }))),
            "{policy:?}"
        );
        assert!(
            policy["spec"]["ingress"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |rule| rule["from"]
                        .as_array()
                        .is_some_and(|from| from.iter().any(
                            |peer| peer["podSelector"]["matchLabels"]["app.kubernetes.io/name"]
                                == "match-marketdata"
                        ))
                        && rule["ports"].as_array().is_some_and(|ports| ports
                            .iter()
                            .any(|port| { port["protocol"] == "UDP" && port["port"] == 24001 }))
                ),
            "{policy:?}"
        );
    }

    #[test]
    fn supervisor_children_receive_market_data_config() {
        use crate::model::{
            MarketDataFeedGroup, MarketDataService, MarketDataServiceSpec, MarketDataTransport,
            MarketDataWebSocket,
        };
        let mut catalog = demo_catalog();
        let shard = catalog.shards.get_mut("fx-eu-01").expect("fx-eu-01");
        shard.spec.packing.mode = PackingMode::Supervisor;
        shard.spec.packing.max_processes_per_pod = 4;
        let service = MarketDataService {
            name: "fx-eu-01-marketdata".to_string(),
            namespace: "match-eu-1".to_string(),
            spec: MarketDataServiceSpec {
                cell: "eu-1".to_string(),
                symbols: Vec::new(),
                feed_groups: vec![
                    MarketDataFeedGroup {
                        name: "eurusd".to_string(),
                        symbols: vec!["eurusd".to_string()],
                        stream_base: 2001,
                        channel: None,
                    },
                    MarketDataFeedGroup {
                        name: "gbpusd".to_string(),
                        symbols: vec!["gbpusd".to_string()],
                        stream_base: 2011,
                        channel: None,
                    },
                ],
                image: "registry.example.com/matchengine-marketdata@sha256:abc".to_string(),
                release: Default::default(),
                replicas: 2,
                transport: MarketDataTransport {
                    channel: Some(
                        "aeron:udp?endpoint=239.1.2.65:24031|interface={pod_ip}|ttl=1".to_string(),
                    ),
                    ..Default::default()
                },
                websocket: MarketDataWebSocket::default(),
                autoscaling: Default::default(),
                resources: None,
                paused: false,
            },
            status: Default::default(),
        };
        catalog
            .market_data_services
            .insert(service.name.clone(), service);
        let objects = render_shard(&catalog, "fx-eu-01", &RenderOptions::default()).unwrap();
        let config = object(&objects, "ConfigMap");
        let processes: Value =
            serde_json::from_str(config["data"]["processes.json"].as_str().unwrap()).unwrap();
        let rows = processes["processes"].as_array().unwrap();
        let eurusd = rows.iter().find(|row| row["name"] == "eurusd").unwrap();
        assert_eq!(eurusd["marketData"]["streamBase"], 2001);
        assert_eq!(
            eurusd["marketData"]["channel"],
            "aeron:udp?endpoint=239.1.2.65:24031|interface={pod_ip}|ttl=1"
        );
        assert_eq!(eurusd["marketData"]["dir"], "/dev/shm/aeron-eurusd");
        let gbpusd = rows.iter().find(|row| row["name"] == "gbpusd").unwrap();
        assert_eq!(gbpusd["marketData"]["streamBase"], 2011);

        let stateful_set = object(&objects, "StatefulSet");
        let pod = &stateful_set["spec"]["template"]["spec"];
        assert!(
            pod["volumes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|volume| volume["name"] == "aeron"),
            "{pod:?}"
        );
        let container = &pod["containers"][0];
        assert!(
            container["volumeMounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|mount| mount["name"] == "aeron" && mount["mountPath"] == "/dev/shm"),
            "{container:?}"
        );
        assert!(
            container["env"]
                .as_array()
                .unwrap()
                .iter()
                .any(|env| env["name"] == "POD_IP"),
            "{container:?}"
        );
    }

    #[test]
    fn paused_market_data_scales_to_zero() {
        let mut catalog = demo_catalog();
        let service = demo_market_data(&mut catalog, "btc-marketdata");
        catalog
            .market_data_services
            .get_mut("btc-marketdata")
            .unwrap()
            .spec
            .paused = true;
        assert!(!service.spec.paused);
        let objects = render_market_data_service(&catalog, "btc-marketdata", None).unwrap();
        let deployment = object(&objects, "Deployment");
        assert_eq!(deployment["spec"]["replicas"], 0);
    }

    #[test]
    fn market_data_refuses_an_even_multicast_address() {
        let mut catalog = demo_catalog();
        demo_market_data(&mut catalog, "btc-marketdata");
        catalog
            .market_data_services
            .get_mut("btc-marketdata")
            .unwrap()
            .spec
            .transport
            .channel = Some("aeron:udp?endpoint=239.1.2.50:24001|interface={pod_ip}|ttl=1".into());
        let error = render_market_data_service(&catalog, "btc-marketdata", None).unwrap_err();
        assert_eq!(error.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.code == "market_data_channel_even")
        );
    }

    #[test]
    fn market_data_refuses_a_wire_version_mismatch() {
        let mut catalog = demo_catalog();
        let mut service = demo_market_data(&mut catalog, "btc-marketdata");
        service.spec.release.wire_version = 6;
        catalog
            .market_data_services
            .insert(service.name.clone(), service);
        let error = render_market_data_service(&catalog, "btc-marketdata", None).unwrap_err();
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.code == "market_data_release")
        );
    }
}
