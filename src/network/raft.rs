use std::io::Cursor;

use actix_web::Responder;
use actix_web::post;
use actix_web::web::Data;
use actix_web::web::Json;
use openraft::RaftTypeConfig;
use openraft::alias::SnapshotMetaOf;
use openraft::alias::SnapshotOf;
use openraft::alias::VoteOf;
use openraft::errors::NetworkError;
use openraft::errors::StreamingError;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::VoteRequest;
use serde::Deserialize;
use serde::Serialize;

use crate::MatchEngineTypeConfig;
use crate::app::MatchEngineApp;

/// Full snapshot transfer request, replacing openraft's chunked v1 `install_snapshot` RPC.
///
/// The leader sends the whole snapshot (meta + serialized state machine) in one request; the
/// follower rebuilds a `Cursor<Vec<u8>>` from `data` and installs it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct FullSnapshotRequest<C: RaftTypeConfig> {
    pub vote: VoteOf<C>,
    pub meta: SnapshotMetaOf<C>,
    pub data: Vec<u8>,
}

// --- Raft communication

#[post("/vote")]
pub async fn vote(
    app: Data<MatchEngineApp>,
    req: Json<VoteRequest<MatchEngineTypeConfig>>,
) -> actix_web::Result<impl Responder> {
    let res = app.raft.vote(req.0).await;
    Ok(Json(res))
}

#[post("/append")]
pub async fn append(
    app: Data<MatchEngineApp>,
    req: Json<AppendEntriesRequest<MatchEngineTypeConfig>>,
) -> actix_web::Result<impl Responder> {
    let res = app.raft.append_entries(req.0).await;
    Ok(Json(res))
}

#[post("/snapshot")]
pub async fn snapshot(
    app: Data<MatchEngineApp>,
    req: Json<FullSnapshotRequest<MatchEngineTypeConfig>>,
) -> actix_web::Result<impl Responder> {
    let snapshot = SnapshotOf::<MatchEngineTypeConfig, Cursor<Vec<u8>>> {
        meta: req.meta.clone(),
        snapshot: Cursor::new(req.data.clone()),
    };

    let res: Result<_, StreamingError<MatchEngineTypeConfig>> =
        match app.raft.install_full_snapshot(req.vote, snapshot).await {
            Ok(resp) => Ok(resp),
            Err(e) => Err(StreamingError::Network(NetworkError::new(&e))),
        };

    Ok(Json(res))
}
