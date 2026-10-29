use std::io::Cursor;

use actix_web::Responder;
use actix_web::post;
use actix_web::web;
use actix_web::web::Data;
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
use serde::de::DeserializeOwned;

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
    req: web::Json<VoteRequest<MatchEngineTypeConfig>>,
) -> actix_web::Result<impl Responder> {
    let res = app.raft.vote(req.0).await;
    Ok(web::Json(res))
}

#[post("/append")]
pub async fn append(
    app: Data<MatchEngineApp>,
    body: web::Payload,
) -> actix_web::Result<impl Responder> {
    let req: AppendEntriesRequest<MatchEngineTypeConfig> = read_json_body(
        body,
        crate::network::MAX_HTTP_RAFT_BODY_BYTES,
        "AppendEntries",
    )
    .await?;
    let res = app.raft.append_entries(req).await;
    Ok(web::Json(res))
}

#[post("/snapshot")]
pub async fn snapshot(
    app: Data<MatchEngineApp>,
    body: web::Payload,
) -> actix_web::Result<impl Responder> {
    let req: FullSnapshotRequest<MatchEngineTypeConfig> =
        read_json_body(body, crate::network::MAX_SNAPSHOT_BYTES, "snapshot").await?;
    let snapshot = SnapshotOf::<MatchEngineTypeConfig, Cursor<Vec<u8>>> {
        meta: req.meta,
        snapshot: Cursor::new(req.data),
    };

    let res: Result<_, StreamingError<MatchEngineTypeConfig>> =
        match app.raft.install_full_snapshot(req.vote, snapshot).await {
            Ok(resp) => Ok(resp),
            Err(e) => Err(StreamingError::Network(NetworkError::new(&e))),
        };

    Ok(web::Json(res))
}

/// Reads a bounded JSON body for the large Raft RPCs.
///
/// `Json<T>` uses the application's small request limit. Snapshot and append
/// bodies are larger, so they are collected here with an explicit cap before
/// serde allocates the decoded value.
async fn read_json_body<T: DeserializeOwned>(
    mut body: web::Payload,
    limit: usize,
    what: &str,
) -> actix_web::Result<T> {
    use futures::StreamExt;

    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(actix_web::error::ErrorBadRequest)?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(actix_web::error::ErrorPayloadTooLarge(format!(
                "{what} body exceeds {limit} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }

    serde_json::from_slice(&bytes)
        .map_err(|e| actix_web::error::ErrorBadRequest(format!("invalid {what} JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::FromRequest;
    use actix_web::http::StatusCode;
    use actix_web::test::TestRequest;

    async fn payload(body: &'static str) -> web::Payload {
        let (request, mut payload) = TestRequest::post()
            .set_payload(body)
            .to_srv_request()
            .into_parts();
        web::Payload::from_request(&request, &mut payload)
            .await
            .unwrap()
    }

    #[actix_web::test]
    async fn raft_json_body_cap_is_enforced() {
        let error = read_json_body::<serde_json::Value>(payload("12345").await, 4, "test")
            .await
            .unwrap_err();
        assert_eq!(
            StatusCode::PAYLOAD_TOO_LARGE,
            error.as_response_error().status_code()
        );

        let value: serde_json::Value = read_json_body(payload(r#"{"ok":true}"#).await, 64, "test")
            .await
            .unwrap();
        assert_eq!(Some(true), value["ok"].as_bool());
    }
}
