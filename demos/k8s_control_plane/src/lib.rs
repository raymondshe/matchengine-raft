//! Reference implementation of the Kubernetes control plane described in
//! `docs/KUBERNETES_CONTROL_PLANE.md`.
//!
//! This crate is a demo, not production code: it keeps the catalog in memory (optionally
//! persisted to a JSON file), does not watch a real cluster, and reports synthetic process
//! status unless an operator feeds it observations. It does implement the parts that are easy
//! to get wrong and expensive to retrofit:
//!
//! - the CRD-shaped data model ([`model`]),
//! - preflight/placement validation ([`validate`]),
//! - deterministic StatefulSet/Service/PDB/NetworkPolicy rendering ([`render`]),
//! - a manager API and GUI ([`api`]), and
//! - the operator's bootstrap/upgrade decision logic ([`reconcile`]).
//!
//! The engine remains single-symbol per process. Packing is N engine containers per pod.

pub mod api;
pub mod bundle;
pub mod error;
pub mod gui;
pub mod model;
pub mod provision;
pub mod reconcile;
pub mod render;
pub mod store;
pub mod validate;

/// API group used by the CRD examples in the design document.
pub const CRD_GROUP: &str = "matching.example.com";
/// API version used by the CRD examples in the design document.
pub const CRD_VERSION: &str = "v1alpha1";

/// Default container image for rendered manifests when a bundle omits one.
pub const DEFAULT_IMAGE: &str = "registry.example.com/matchengine-raft:1.4.0-nonroot";
