use std::collections::BTreeMap;
use std::collections::BTreeSet;

use actix_web::get;
use actix_web::post;
use actix_web::web;
use actix_web::web::Data;
use actix_web::Responder;
use openraft::error::Infallible;
use openraft::Node;
use openraft::RaftMetrics;
use web::Json;

use crate::app::ExampleApp;
use crate::ExampleNodeId;
use crate::ExampleTypeConfig;

// --- Cluster management endpoints
//
// These endpoints are used to administer the Raft cluster, including
// initializing the cluster, adding/removing nodes, and monitoring status.

/// Adds a node as a **Learner** to the cluster.
///
/// A Learner receives log replication from the leader but does not vote
/// in leader elections or participate in quorum decisions. This is the
/// first step in adding a new node to the cluster - it allows the new
/// node to catch up with the leader's state before being promoted to
/// a full voting member.
///
/// # Arguments
///
/// * A tuple of `(node_id, address)` where address is in "host:port" format
///
/// # Returns
///
/// The result of adding the learner, including information about whether
/// the node is already known or newly added.
#[post("/add-learner")]
pub async fn add_learner(
    app: Data<ExampleApp>,
    req: Json<(ExampleNodeId, String)>,
) -> actix_web::Result<impl Responder> {
    let node_id = req.0 .0;
    let node = Node {
        addr: req.0 .1.clone(),
        ..Default::default()
    };
    let res = app.raft.add_learner(node_id, Some(node), true).await;
    Ok(Json(res))
}

/// Changes cluster membership by promoting learners or removing members.
///
/// This endpoint reconfigures the set of voting members in the cluster.
/// All nodes in the new membership set must have already been added as
/// learners and caught up with replication.
///
/// Membership changes happen safely in two phases to ensure consensus
/// is maintained throughout the transition.
///
/// # Arguments
///
/// * A set of node IDs that should be the new voting members
///
/// # Returns
///
/// The result of the membership change, including the final configuration.
#[post("/change-membership")]
pub async fn change_membership(
    app: Data<ExampleApp>,
    req: Json<BTreeSet<ExampleNodeId>>,
) -> actix_web::Result<impl Responder> {
    let res = app.raft.change_membership(req.0, true, false).await;
    Ok(Json(res))
}

/// Initializes a single-node cluster.
///
/// This must be called once on a fresh cluster to bootstrap the Raft
/// protocol. It creates an initial cluster configuration containing only
/// this node and marks it as the leader.
///
/// Once initialized, additional nodes can be added using `add-learner`
/// followed by `change-membership`.
///
/// # Returns
///
/// The result of initialization - will fail if the cluster is already initialized.
#[post("/init")]
pub async fn init(app: Data<ExampleApp>) -> actix_web::Result<impl Responder> {
    let mut nodes = BTreeMap::new();
    nodes.insert(app.id, Node {
        addr: app.addr.clone(),
        data: Default::default(),
    });
    let res = app.raft.initialize(nodes).await;
    Ok(Json(res))
}

/// Gets the latest metrics about this Raft node.
///
/// Metrics include information like:
/// - Current leader ID
/// - Current term
/// - Last applied log index
/// - Cluster membership configuration
/// - Replication status for each follower
///
/// This endpoint is useful for monitoring the health and status of the cluster.
///
/// # Returns
///
/// A `RaftMetrics` structure with comprehensive status information.
#[get("/metrics")]
pub async fn metrics(app: Data<ExampleApp>) -> actix_web::Result<impl Responder> {
    let metrics = app.raft.metrics().borrow().clone();

    let res: Result<RaftMetrics<ExampleTypeConfig>, Infallible> = Ok(metrics);
    Ok(Json(res))
}
