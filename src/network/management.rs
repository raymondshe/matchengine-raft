use std::collections::BTreeMap;
use std::collections::BTreeSet;

use actix_web::Responder;
use actix_web::get;
use actix_web::post;
use actix_web::web::Data;
use actix_web::web::Json;
use openraft::BasicNode;
use openraft::RaftMetrics;
use openraft::ReadPolicy;
use openraft::alias::LogIdOf;
use openraft::async_runtime::WatchReceiver;
use openraft::errors::Infallible;
use openraft::errors::decompose::DecomposeResult;
use openraft::raft::linearizable_read::ReadLogId;
use serde::Deserialize;
use serde::Serialize;

use crate::MatchEngineNodeId;
use crate::MatchEngineTypeConfig;
use crate::app::MatchEngineApp;

/// Serializable representation of linearizer data for follower reads
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearizerData {
    pub node_id: MatchEngineNodeId,
    /// Serializes transparently as a `LogId`.
    pub read_log_id: ReadLogId<MatchEngineTypeConfig>,
    pub applied: Option<LogIdOf<MatchEngineTypeConfig>>,
}

// --- Cluster management

/// Add a node as **Learner**.
///
/// A Learner receives log replication from the leader but does not vote.
/// This should be done before adding a node as a member into the cluster
/// (by calling `change-membership`)
#[post("/add-learner")]
pub async fn add_learner(app: Data<MatchEngineApp>, req: Json<(MatchEngineNodeId, String)>) -> actix_web::Result<impl Responder> {
    let node_id = req.0.0;
    let node = BasicNode { addr: req.0.1.clone() };
    let res = app.raft.add_learner(node_id, node, true).await.decompose().unwrap();
    Ok(Json(res))
}

/// Changes specified learners to members, or remove members.
#[post("/change-membership")]
pub async fn change_membership(app: Data<MatchEngineApp>, req: Json<BTreeSet<MatchEngineNodeId>>) -> actix_web::Result<impl Responder> {
    let res = app.raft.change_membership(req.0, false).await.decompose().unwrap();
    Ok(Json(res))
}

/// Initialize a single-node cluster if the `req` is empty vec.
/// Otherwise initialize a cluster with the `req` specified vec of node-id and node-address
#[post("/init")]
pub async fn init(app: Data<MatchEngineApp>, req: Json<Vec<(MatchEngineNodeId, String)>>) -> actix_web::Result<impl Responder> {
    let mut nodes = BTreeMap::new();
    if req.0.is_empty() {
        nodes.insert(app.id, BasicNode { addr: app.addr.clone() });
    } else {
        for (id, addr) in req.0.into_iter() {
            nodes.insert(id, BasicNode { addr });
        }
    };
    let res = app.raft.initialize(nodes).await.decompose().unwrap();
    Ok(Json(res))
}

/// Get the latest metrics of the cluster
#[get("/metrics")]
pub async fn metrics(app: Data<MatchEngineApp>) -> actix_web::Result<impl Responder> {
    let metrics = app.raft.metrics().borrow_watched().clone();

    let res: Result<RaftMetrics<MatchEngineTypeConfig>, Infallible> = Ok(metrics);
    Ok(Json(res))
}

/// Get linearizer data for performing linearizable reads on followers
///
/// This endpoint is used by followers to obtain linearizer data from the leader.
/// The follower can then reconstruct a Linearizer and wait for its local state
/// machine to catch up before performing a linearizable read.
#[post("/get_linearizer")]
pub async fn get_linearizer(app: Data<MatchEngineApp>) -> actix_web::Result<impl Responder> {
    let linearizer = app.raft.get_read_linearizer(ReadPolicy::ReadIndex).await.decompose().unwrap();

    let data = match linearizer {
        Ok(lin) => {
            let data = LinearizerData {
                node_id: *lin.node_id(),
                read_log_id: *lin.read_log_id(),
                applied: lin.applied().cloned(),
            };
            Ok(data)
        }
        Err(e) => Err(e),
    };

    Ok(Json(data))
}
