//! Engine image inventory for the upgrade picker.
//!
//! The manager lists images cached by the Kubernetes nodes (`Node.status.images`) and images used
//! by running engine pods. The result is digest-pinned where possible, carries every tag known for
//! the image, and records which shards are running it. Nothing here mutates the cluster.

use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::Client;
use kube::api::{Api, ListParams};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::store::Catalog;

/// One selectable engine image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageChoice {
    /// Digest-pinned reference where the digest is known.
    pub reference: String,
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default)]
    pub size_bytes: u64,
    /// Number of nodes that have this image cached.
    #[serde(default)]
    pub nodes: u32,
    /// Shards whose pods currently run this image.
    #[serde(default)]
    pub running_shards: Vec<String>,
    #[serde(default)]
    pub running_pods: u32,
    /// The image currently desired by the requested shard.
    #[serde(default)]
    pub current: bool,
    /// At least one pod of the requested shard runs this image.
    #[serde(default)]
    pub running_here: bool,
}

impl ImageChoice {
    /// True when `reference` names this image, by digest, exact reference or known tag.
    pub fn matches_reference(&self, reference: &str) -> bool {
        if same_image(&self.reference, reference) {
            return true;
        }
        parse_image(reference)
            .and_then(|parsed| parsed.tag)
            .is_some_and(|tag| self.tags.iter().any(|known| known == &tag))
    }
}

/// Response for the upgrade image picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInventory {
    /// Selectable images, running/current first.
    pub items: Vec<ImageChoice>,
    /// The image currently written in the shard spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Repository shared by the catalog and the shard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Set when the Kubernetes API could not be listed; catalog references are still returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// A parsed image reference.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ParsedImage {
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl ParsedImage {
    /// Canonical digest-pinned reference when possible, otherwise the tag reference.
    pub fn reference(&self) -> String {
        match &self.digest {
            Some(digest) => format!("{}@{}", self.repository, digest),
            None => match &self.tag {
                Some(tag) => format!("{}:{}", self.repository, tag),
                None => self.repository.clone(),
            },
        }
    }

    fn key(&self) -> String {
        self.reference()
    }
}

/// Parse `repo`, `repo:tag` or `repo@sha256:digest`.
pub fn parse_image(input: &str) -> Option<ParsedImage> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }
    let (repository, digest) = match raw.split_once('@') {
        Some((repository, digest))
            if !repository.is_empty() && digest.starts_with("sha256:") && digest.len() > 7 =>
        {
            (repository.to_string(), Some(digest.to_string()))
        }
        _ => (raw.to_string(), None),
    };
    if digest.is_some() {
        return Some(ParsedImage {
            repository,
            tag: None,
            digest,
        });
    }
    // A colon before the last slash belongs to a registry port, not a tag.
    let last_slash = repository.rfind('/').map(|index| index + 1).unwrap_or(0);
    match repository[last_slash..].rfind(':') {
        Some(offset) => {
            let index = last_slash + offset;
            let tag = repository[index + 1..].to_string();
            if tag.is_empty() {
                return Some(ParsedImage {
                    repository: repository[..index].to_string(),
                    tag: None,
                    digest: None,
                });
            }
            Some(ParsedImage {
                repository: repository[..index].to_string(),
                tag: Some(tag),
                digest: None,
            })
        }
        None => Some(ParsedImage {
            repository,
            tag: None,
            digest: None,
        }),
    }
}

/// True when two references identify the same image.
pub fn same_image(left: &str, right: &str) -> bool {
    match (parse_image(left), parse_image(right)) {
        (Some(left), Some(right)) => match (&left.digest, &right.digest) {
            (Some(left), Some(right)) => left == right,
            _ => left.repository == right.repository && left.tag == right.tag,
        },
        _ => left.trim() == right.trim(),
    }
}

/// Repositories referenced by the catalog.
pub fn catalog_repositories(catalog: &Catalog) -> BTreeSet<String> {
    let mut repositories = BTreeSet::new();
    for shard in catalog.shards.values() {
        if let Some(image) = parse_image(&shard.spec.image) {
            repositories.insert(image.repository);
        }
    }
    for deployment in catalog.deployments.values() {
        if let Some(reference) = deployment.spec.image.as_deref()
            && let Some(image) = parse_image(reference)
        {
            repositories.insert(image.repository);
        }
    }
    for service in catalog.market_data_services.values() {
        if let Some(image) = parse_image(&service.spec.image) {
            repositories.insert(image.repository);
        }
    }
    for system in catalog.order_systems.values() {
        if let Some(image) = parse_image(&system.spec.image) {
            repositories.insert(image.repository);
        }
    }
    repositories
}

/// Images referenced by the catalog, even when no pod or node cache reports them.
pub fn catalog_images(catalog: &Catalog) -> Vec<ImageChoice> {
    let mut images = BTreeMap::new();
    merge_catalog(catalog, &mut images);
    images.into_values().collect()
}

/// Add catalog image references to an existing inventory.
pub fn merge_catalog(catalog: &Catalog, images: &mut BTreeMap<String, ImageChoice>) {
    for shard in catalog.shards.values() {
        insert_reference(images, &shard.spec.image);
    }
    for deployment in catalog.deployments.values() {
        if let Some(reference) = deployment.spec.image.as_deref() {
            insert_reference(images, reference);
        }
    }
    for service in catalog.market_data_services.values() {
        insert_reference(images, &service.spec.image);
    }
    for system in catalog.order_systems.values() {
        insert_reference(images, &system.spec.image);
    }
}

/// Merge cluster-discovered images into the catalog inventory.
///
/// A digest-pinned cluster entry supersedes a tag-only catalog placeholder for the same tag, so the
/// picker never shows `repo:tag` and `repo@sha256:...` as two different images.
pub fn merge_cluster(catalog: &mut BTreeMap<String, ImageChoice>, cluster: Vec<ImageChoice>) {
    for mut image in cluster {
        if image.digest.is_some() {
            let tags = image.tags.clone();
            let superseded: Vec<String> = catalog
                .iter()
                .filter(|(_, existing)| {
                    existing.digest.is_none()
                        && existing.repository == image.repository
                        && existing
                            .tag
                            .as_ref()
                            .is_some_and(|tag| tags.iter().any(|known| known == tag))
                })
                .map(|(key, _)| key.clone())
                .collect();
            for key in superseded {
                if let Some(existing) = catalog.remove(&key) {
                    for tag in existing.tags {
                        add_tag(&mut image, &tag);
                    }
                    for shard in existing.running_shards {
                        if !image.running_shards.contains(&shard) {
                            image.running_shards.push(shard);
                        }
                    }
                }
            }
        }
        if let Some(existing) = catalog.get_mut(&image.reference) {
            existing.nodes = existing.nodes.max(image.nodes);
            existing.size_bytes = existing.size_bytes.max(image.size_bytes);
            existing.running_pods = existing.running_pods.max(image.running_pods);
            if let Some(tag) = &image.tag {
                add_tag(existing, tag);
            }
            for tag in image.tags {
                add_tag(existing, &tag);
            }
            for shard in image.running_shards {
                if !existing.running_shards.contains(&shard) {
                    existing.running_shards.push(shard);
                }
            }
        } else {
            catalog.insert(image.reference.clone(), image);
        }
    }
}

/// Mark the shard's current image and the image its pods run, then sort for display.
pub fn annotate(images: &mut [ImageChoice], current: Option<&str>, shard: Option<&str>) {
    for image in images.iter_mut() {
        image.current = current.is_some_and(|current| image.matches_reference(current));
        image.running_here =
            shard.is_some_and(|shard| image.running_shards.iter().any(|known| known == shard));
    }
    sort_images(images);
}

/// Fetch node-cached and running images from the Kubernetes API.
pub async fn cluster_images(
    client: &Client,
    repositories: &BTreeSet<String>,
) -> Result<Vec<ImageChoice>, ApiError> {
    let nodes = Api::<Node>::all(client.clone())
        .list(&ListParams::default())
        .await
        .map_err(|error| ApiError::internal(format!("list nodes for images: {error}")))?;
    let mut pods = Api::<Pod>::all(client.clone())
        .list(&ListParams::default().labels("matching.io/cluster"))
        .await
        .map_err(|error| ApiError::internal(format!("list pods for images: {error}")))?;
    let market_data_pods = Api::<Pod>::all(client.clone())
        .list(&ListParams::default().labels("matching.io/market-data"))
        .await
        .map_err(|error| {
            ApiError::internal(format!("list market-data pods for images: {error}"))
        })?;
    pods.items.extend(market_data_pods.items);
    let order_system_pods = Api::<Pod>::all(client.clone())
        .list(&ListParams::default().labels("matching.io/order-system"))
        .await
        .map_err(|error| {
            ApiError::internal(format!("list order-system pods for images: {error}"))
        })?;
    pods.items.extend(order_system_pods.items);

    let mut images = BTreeMap::<String, ImageChoice>::new();
    for node in &nodes.items {
        let Some(status) = node.status.as_ref() else {
            continue;
        };
        let mut seen = BTreeSet::new();
        for image in status.images.as_deref().unwrap_or_default() {
            let names: Vec<ParsedImage> = image
                .names
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(|name| parse_image(name))
                .collect();
            let Some(primary) = names
                .iter()
                .find(|image| image.digest.is_some())
                .or_else(|| names.first())
            else {
                continue;
            };
            if !allowed_repository(&primary.repository, repositories) {
                continue;
            }
            let key = primary.key();
            let entry = images.entry(key.clone()).or_insert_with(|| ImageChoice {
                reference: primary.reference(),
                repository: primary.repository.clone(),
                tag: primary.tag.clone(),
                tags: Vec::new(),
                digest: primary.digest.clone(),
                size_bytes: image.size_bytes.unwrap_or(0).max(0) as u64,
                nodes: 0,
                running_shards: Vec::new(),
                running_pods: 0,
                current: false,
                running_here: false,
            });
            if seen.insert(key) {
                entry.nodes = entry.nodes.saturating_add(1);
            }
            entry.size_bytes = entry
                .size_bytes
                .max(image.size_bytes.unwrap_or(0).max(0) as u64);
            for candidate in &names {
                if let (Some(digest), Some(tag)) = (&candidate.digest, &candidate.tag) {
                    if Some(digest) == entry.digest.as_ref() {
                        add_tag(entry, tag);
                    }
                } else if let Some(tag) = &candidate.tag
                    && candidate.digest.is_none()
                {
                    add_tag(entry, tag);
                }
            }
        }
    }

    for pod in &pods.items {
        let Some(status) = pod.status.as_ref() else {
            continue;
        };
        if status
            .phase
            .as_deref()
            .is_some_and(|phase| phase == "Succeeded" || phase == "Failed")
        {
            continue;
        }
        let shard = pod
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get("matching.io/cluster"))
            .or_else(|| {
                pod.metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("matching.io/shard"))
            })
            .or_else(|| {
                pod.metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("matching.io/market-data"))
            })
            .or_else(|| {
                pod.metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("matching.io/order-system"))
            })
            .cloned()
            .or_else(|| pod.metadata.namespace.clone());
        for container in status.container_statuses.as_deref().unwrap_or_default() {
            let requested = if container.image.is_empty() {
                None
            } else {
                parse_image(&container.image)
            };
            let runtime = parse_image(strip_runtime_prefix(&container.image_id));
            let Some(mut parsed) = requested.or_else(|| runtime.clone()) else {
                continue;
            };
            // Prefer the runtime digest so a tag-only pod spec still joins the node-cache entry.
            if parsed.digest.is_none()
                && let Some(runtime) = runtime.as_ref()
                && runtime.digest.is_some()
            {
                parsed.digest = runtime.digest.clone();
            }
            if !allowed_repository(&parsed.repository, repositories) {
                continue;
            }
            let key = parsed.key();
            let entry = images.entry(key).or_insert_with(|| ImageChoice {
                reference: parsed.reference(),
                repository: parsed.repository.clone(),
                tag: parsed.tag.clone(),
                tags: Vec::new(),
                digest: parsed.digest.clone(),
                size_bytes: 0,
                nodes: 0,
                running_shards: Vec::new(),
                running_pods: 0,
                current: false,
                running_here: false,
            });
            entry.running_pods = entry.running_pods.saturating_add(1);
            if let Some(tag) = &parsed.tag {
                add_tag(entry, tag);
            }
            if let Some(shard) = &shard
                && !entry.running_shards.contains(shard)
            {
                entry.running_shards.push(shard.clone());
            }
        }
    }

    let mut images: Vec<ImageChoice> = images.into_values().collect();
    for image in &mut images {
        image.running_shards.sort();
    }
    Ok(images)
}

/// Sort running/current images first, then cached images, then by tag or reference.
pub fn sort_images(images: &mut [ImageChoice]) {
    images.sort_by(|left, right| {
        right
            .running_here
            .cmp(&left.running_here)
            .then_with(|| right.current.cmp(&left.current))
            .then_with(|| right.running_pods.cmp(&left.running_pods))
            .then_with(|| right.nodes.cmp(&left.nodes))
            .then_with(|| {
                left.tag
                    .as_deref()
                    .unwrap_or(&left.reference)
                    .cmp(right.tag.as_deref().unwrap_or(&right.reference))
            })
            .then_with(|| left.reference.cmp(&right.reference))
    });
}

fn insert_reference(images: &mut BTreeMap<String, ImageChoice>, reference: &str) {
    let Some(parsed) = parse_image(reference) else {
        return;
    };
    let key = parsed.key();
    let entry = images.entry(key).or_insert_with(|| ImageChoice {
        reference: parsed.reference(),
        repository: parsed.repository.clone(),
        tag: parsed.tag.clone(),
        tags: Vec::new(),
        digest: parsed.digest.clone(),
        size_bytes: 0,
        nodes: 0,
        running_shards: Vec::new(),
        running_pods: 0,
        current: false,
        running_here: false,
    });
    if let Some(tag) = &parsed.tag {
        add_tag(entry, tag);
    }
}

fn add_tag(entry: &mut ImageChoice, tag: &str) {
    if !entry.tags.iter().any(|existing| existing == tag) {
        entry.tags.push(tag.to_string());
        entry.tags.sort();
    }
    if entry.tag.is_none() {
        entry.tag = Some(tag.to_string());
    }
}

fn allowed_repository(repository: &str, repositories: &BTreeSet<String>) -> bool {
    if repositories.is_empty() {
        return basename(repository) == "matchengine-raft";
    }
    repositories
        .iter()
        .any(|allowed| basename(allowed) == basename(repository))
}

fn basename(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

fn strip_runtime_prefix(image_id: &str) -> &str {
    for prefix in ["docker-pullable://", "docker://", "containerd://"] {
        if let Some(value) = image_id.strip_prefix(prefix) {
            return value;
        }
    }
    image_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags_digests_and_registry_ports() {
        let tagged = parse_image("matchengine-raft:k8s-rollout-v3").unwrap();
        assert_eq!(tagged.repository, "matchengine-raft");
        assert_eq!(tagged.tag.as_deref(), Some("k8s-rollout-v3"));
        assert_eq!(tagged.digest, None);

        let digest = parse_image("matchengine-raft@sha256:8332448faa97").unwrap();
        assert_eq!(digest.repository, "matchengine-raft");
        assert_eq!(digest.digest.as_deref(), Some("sha256:8332448faa97"));

        let registry = parse_image("registry.example.com:5000/team/matchengine-raft:v1").unwrap();
        assert_eq!(
            registry.repository,
            "registry.example.com:5000/team/matchengine-raft"
        );
        assert_eq!(registry.tag.as_deref(), Some("v1"));

        assert_eq!(
            parse_image("registry.example.com:5000/team/matchengine-raft")
                .unwrap()
                .repository,
            "registry.example.com:5000/team/matchengine-raft"
        );
    }

    #[test]
    fn matches_digests_before_tags() {
        assert!(same_image(
            "matchengine-raft@sha256:abc",
            "other/matchengine-raft@sha256:abc"
        ));
        assert!(same_image(
            "matchengine-raft:k8s-rollout-v3",
            "matchengine-raft:k8s-rollout-v3"
        ));
        assert!(!same_image(
            "matchengine-raft:k8s-rollout-v3",
            "matchengine-raft@sha256:abc"
        ));
    }

    #[test]
    fn catalog_images_are_digest_pinned_and_deduplicated() {
        let catalog = crate::store::demo_catalog();
        let images = catalog_images(&catalog);
        assert!(!images.is_empty());
        assert!(images.iter().all(|image| !image.reference.is_empty()));
        let references: BTreeSet<_> = images.iter().map(|image| image.reference.clone()).collect();
        assert_eq!(references.len(), images.len());
    }

    #[test]
    fn running_images_sort_first() {
        let mut images = vec![
            ImageChoice {
                reference: "matchengine-raft:old".to_string(),
                repository: "matchengine-raft".to_string(),
                tag: Some("old".to_string()),
                tags: vec!["old".to_string()],
                digest: None,
                size_bytes: 1,
                nodes: 1,
                running_shards: Vec::new(),
                running_pods: 0,
                current: false,
                running_here: false,
            },
            ImageChoice {
                reference: "matchengine-raft:new".to_string(),
                repository: "matchengine-raft".to_string(),
                tag: Some("new".to_string()),
                tags: vec!["new".to_string()],
                digest: None,
                size_bytes: 1,
                nodes: 1,
                running_shards: vec!["me-btcusdt".to_string()],
                running_pods: 4,
                current: false,
                running_here: true,
            },
        ];
        sort_images(&mut images);
        assert_eq!(images[0].tag.as_deref(), Some("new"));
    }

    fn choice(reference: &str, tags: &[&str], shards: &[&str]) -> ImageChoice {
        let parsed = parse_image(reference).expect("valid reference");
        ImageChoice {
            reference: parsed.reference(),
            repository: parsed.repository,
            tag: parsed.tag,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            digest: parsed.digest,
            size_bytes: 1,
            nodes: 1,
            running_shards: shards.iter().map(|shard| shard.to_string()).collect(),
            running_pods: shards.len() as u32,
            current: false,
            running_here: false,
        }
    }

    #[test]
    fn digest_pinned_cluster_image_supersedes_a_tag_only_catalog_entry() {
        let mut catalog = BTreeMap::new();
        insert_reference(
            &mut catalog,
            "registry.example.com/matchengine-raft:1.4.0-nonroot",
        );
        merge_cluster(
            &mut catalog,
            vec![choice(
                "registry.example.com/matchengine-raft@sha256:abc",
                &["1.4.0-nonroot"],
                &["fx-eu-01"],
            )],
        );
        assert_eq!(catalog.len(), 1);
        let image = catalog.values().next().unwrap();
        assert_eq!(
            image.reference,
            "registry.example.com/matchengine-raft@sha256:abc"
        );
        assert_eq!(image.tags, vec!["1.4.0-nonroot"]);
        assert_eq!(image.running_shards, vec!["fx-eu-01"]);
    }

    #[test]
    fn annotate_matches_a_tag_against_the_digest_pinned_inventory() {
        let mut images = vec![choice(
            "registry.example.com/matchengine-raft@sha256:abc",
            &["k8s-rollout-v3"],
            &["me-btcusdt"],
        )];
        annotate(
            &mut images,
            Some("registry.example.com/matchengine-raft:k8s-rollout-v3"),
            Some("me-btcusdt"),
        );
        assert!(images[0].current);
        assert!(images[0].running_here);
        assert!(images[0].matches_reference("matchengine-raft@sha256:abc"));
    }

    #[test]
    fn cached_images_sort_before_uncached_catalog_references() {
        let mut uncached = choice(
            "registry.example.com/matchengine-raft:k8s-uncached",
            &["k8s-uncached"],
            &[],
        );
        uncached.nodes = 0;
        uncached.size_bytes = 0;
        let mut images = vec![
            uncached,
            choice(
                "registry.example.com/matchengine-raft:k8s-cached",
                &["k8s-cached"],
                &[],
            ),
        ];
        sort_images(&mut images);
        assert_eq!(images[0].tag.as_deref(), Some("k8s-cached"));
    }
}
