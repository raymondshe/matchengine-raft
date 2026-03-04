use actix_web::post;
use actix_web::web;
use actix_web::web::Data;
use actix_web::Responder;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::InstallSnapshotRequest;
use openraft::raft::VoteRequest;
use web::Json;

use crate::app::ExampleApp;
use crate::ExampleNodeId;
use crate::ExampleTypeConfig;

// --- Raft internal communication endpoints
//
// These endpoints are used by Raft nodes to communicate with each other
// to maintain consensus. They should not be called directly by clients.

/// Handles incoming vote requests during leader elections.
///
/// When a candidate node starts an election, it sends vote requests to all
/// other nodes. This endpoint processes those requests and determines
/// whether to grant the vote based on the Raft safety rules.
#[post("/raft-vote")]
pub async fn vote(app: Data<ExampleApp>, req: Json<VoteRequest<ExampleNodeId>>) -> actix_web::Result<impl Responder> {
    let res = app.raft.vote(req.0).await;
    Ok(Json(res))
}

/// Handles append entries requests for log replication and heartbeats.
///
/// This is the main RPC used by the leader to:
/// 1. Send periodic heartbeats to maintain leadership
/// 2. Replicate log entries to followers
/// 3. Commit entries once they're safely replicated
#[post("/raft-append")]
pub async fn append(
    app: Data<ExampleApp>,
    req: Json<AppendEntriesRequest<ExampleTypeConfig>>,
) -> actix_web::Result<impl Responder> {
    let res = app.raft.append_entries(req.0).await;
    Ok(Json(res))
}

/// Handles snapshot installation requests.
///
/// When a follower is far behind the leader, the leader may send an entire
/// snapshot instead of individual log entries. This endpoint receives and
/// installs those snapshots to bring the follower up to date efficiently.
#[post("/raft-snapshot")]
pub async fn snapshot(
    app: Data<ExampleApp>,
    req: Json<InstallSnapshotRequest<ExampleTypeConfig>>,
) -> actix_web::Result<impl Responder> {
    let res = app.raft.install_snapshot(req.0).await;
    Ok(Json(res))
}
