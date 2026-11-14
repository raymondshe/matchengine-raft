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
    Cell, PackingMode, RAFT_PORT_OFFSET, Resources, ShardClass, SymbolDeployment, SymbolShard,
    TradingSymbol, dns_label, port_name,
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
    opts: &'a RenderOptions,
    namespace: String,
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

    let deployments: Vec<&SymbolDeployment> = catalog.shard_deployments(&shard.name);
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
        if let Some(route) = catalog.route_for_symbol(&symbol.name) {
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
            json!({
                "name": symbol.name,
                "slot": deployment.spec.placement.slot,
                "httpPort": deployment.spec.placement.http_port,
                "raftPort": deployment.spec.placement.raft_port(),
            })
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
    json!({
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
            ],
        },
    })
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

    let mut pod_spec = json!({
        "automountServiceAccountToken": false,
        "terminationGracePeriodSeconds": 60,
        "securityContext": pod_security_context(ctx),
        "containers": containers,
        "volumes": [{ "name": "tmp", "emptyDir": {} }],
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
                        "matching.io/config-render": "demos/k8s_control_plane",
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
    ];
    if let Some(value) = ctx.shard.spec.raft.wal_fsync.env_value() {
        env.push(json!({ "name": "RAFT_WAL_FSYNC_MS", "value": value }));
    }

    let supervisor = json!({
        "name": "supervisor",
        "image": ctx.shard.spec.image,
        "imagePullPolicy": ctx.image_pull_policy(),
        "command": ["/usr/local/bin/match_supervisor"],
        "args": [
            "--config", "/etc/match/processes.json",
            "--id", "$(POD_ORDINAL)",
            "--pod-fqdn", "$(POD_FQDN)",
            "--data-dir", "/data",
            "--engine-bin", "/usr/local/bin/matchengine-raft",
            "--supervisor-addr", "0.0.0.0:9090",
            "--snapshot-interval-ms", snapshot_ms.to_string(),
            "--read-policy", ctx.shard.spec.raft.read_policy.as_str(),
            "--max-inflight-writes", ctx.shard.spec.quotas.per_process.max_inflight_writes.to_string(),
            "--max-events", ctx.shard.spec.quotas.per_process.max_events.to_string(),
        ],
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
        "volumeMounts": [
            { "name": "data", "mountPath": "/data" },
            { "name": "tmp", "mountPath": "/tmp" },
            { "name": "config", "mountPath": "/etc/match", "readOnly": true },
        ],
    });

    let pod_labels = ctx.labels();
    let mut pod_spec = json!({
        "automountServiceAccountToken": false,
        "terminationGracePeriodSeconds": 60,
        "securityContext": pod_security_context(ctx),
        "containers": [supervisor],
        "volumes": [
            { "name": "tmp", "emptyDir": {} },
            {
                "name": "config",
                "configMap": {
                    "name": format!("{}-shard", ctx.shard.name),
                    "items": [{ "key": "processes.json", "path": "processes.json" }],
                },
            },
        ],
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
                        "matching.io/config-render": "demos/k8s_control_plane",
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
    ];
    if let Some(value) = ctx.shard.spec.raft.wal_fsync.env_value() {
        env.push(json!({ "name": "RAFT_WAL_FSYNC_MS", "value": value }));
    }

    json!({
        "name": dns_label(&symbol.name),
        "image": ctx.image(deployment),
        "imagePullPolicy": ctx.image_pull_policy(),
        "command": ["/usr/local/bin/matchengine-raft"],
        "args": [
            "--id", "$(POD_ORDINAL)",
            "--http-addr", format!("$(POD_FQDN):{http_port}"),
            "--data-dir", "/data",
            "--instance-prefix", dns_label(&symbol.name),
            "--snapshot-interval-ms", snapshot_ms.to_string(),
            "--read-policy", ctx.shard.spec.raft.read_policy.as_str(),
            "--max-inflight-writes", ctx.shard.spec.quotas.per_process.max_inflight_writes.to_string(),
            "--max-events", ctx.shard.spec.quotas.per_process.max_events.to_string(),
        ],
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
        "volumeMounts": [
            { "name": "data", "mountPath": "/data" },
            { "name": "tmp", "mountPath": "/tmp" },
        ],
    })
}

/// Whether a shard is safe to roll one pod at a time for the target image.
///
/// Same-version restarts are always safe. A different wire/snapshot version is stop-the-world
/// unless the target release declares mixed-version support.
pub fn rolling_upgrade_safe(shard: &SymbolShard, target_release: &crate::model::Release) -> bool {
    let current = &shard.spec.release;
    (current.wire_version == target_release.wire_version
        && current.snapshot_version == target_release.snapshot_version)
        || target_release.mixed_version_safe
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
            containers[0]["args"][3], "$(POD_FQDN):21001",
            "http address uses the stable pod DNS name"
        );
        assert_eq!(containers[1]["args"][3], "$(POD_FQDN):21002");
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
}
