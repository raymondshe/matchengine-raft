//! Reference implementation of the Kubernetes control plane described in
//! `docs/KUBERNETES_CONTROL_PLANE.md`.
//!
//! This crate is the reference manager: it keeps an in-memory catalog cache, persists it either
//! as JSON (the offline demo) or as CRDs through the Kubernetes API server (the production path),
//! and reports synthetic process status unless an operator feeds it observations. It implements
//! the parts that are easy to get wrong and expensive to retrofit:
//!
//! - the CRD-shaped data model ([`model`]),
//! - preflight/placement validation ([`validate`]),
//! - deterministic StatefulSet/Service/PDB/NetworkPolicy rendering ([`render`]),
//! - a manager API and GUI ([`api`]),
//! - a CRD-backed store with informer-cache refresh ([`crd`]), and
//! - the operator's bootstrap/upgrade decision logic ([`reconcile`]).
//!
//! The engine remains single-symbol per process. Packing is N engine containers per pod.

pub mod api;
pub mod bundle;
pub mod crd;
pub mod error;
pub mod gui;
pub mod model;
pub mod provision;
pub mod reconcile;
pub mod render;
pub mod store;
pub mod validate;

/// API group used by the catalog CRDs.
pub const CRD_GROUP: &str = "matching.io";
/// API version used by the catalog CRDs.
pub const CRD_VERSION: &str = "v1";

/// Default container image for rendered manifests when a bundle omits one.
pub const DEFAULT_IMAGE: &str = "registry.example.com/matchengine-raft:1.4.0-nonroot";
