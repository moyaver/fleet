// SPDX-License-Identifier: AGPL-3.0-only
//! The agent's three upload calls (see [`fleet_protocol::artifact`]).

use std::{
    io,
    path::{Path as FsPath, PathBuf},
    time::{Duration, SystemTime},
};

use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use fleet_blob::{
    BlobKey, ByteStream, Sha256Digest,
    verify::{self, ArtifactClass as BlobClass, Declared, Kind, Limits, VerifyError},
};
use fleet_core::policy::{Action, Principal, Resource, authorize};
use fleet_protocol::artifact::{
    ArtifactClass, RejectReason, UploadGrant, UploadRequest, UploadResult,
};
use fleet_store::artifacts::{
    Claim, Finished, Pending, PendingArtifact, Quota, Requested, UploadAsk,
};
use fleet_store::gc::BlobReservation;
use futures_util::StreamExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{
    api::{ApiError, error},
    ingest::bearer,
    state::AppState,
};

/// Longest path label kept, and longest single segment (the name).
const MAX_PATH: usize = 512;
const MAX_SEGMENT: usize = 255;

/// The publisher behind the request, or the response that refuses it.
async fn publisher(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<fleet_store::auth::Publisher, Box<Response>> {
    let Some(token) = bearer(headers) else {
        return Err(Box::new(error(
            StatusCode::UNAUTHORIZED,
            "a publisher credential is required",
        )));
    };
    let publisher = match state.store.authenticate(&token).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(Box::new(error(
                StatusCode::UNAUTHORIZED,
                "unknown, expired or revoked credential",
            )));
        }
        Err(e) => return Err(Box::new(internal(&e))),
    };
    let principal = Principal::Publisher {
        project: publisher.project_id,
    };
    if authorize(
        &principal,
        Action::UploadArtifact,
        Resource::Project(publisher.project_id),
    ) != fleet_core::policy::Decision::Allow
    {
        return Err(Box::new(error(
            StatusCode::FORBIDDEN,
            "this credential may not upload artifacts",
        )));
    }
    Ok(publisher)
}

fn internal(e: &dyn std::fmt::Display) -> Response {
    tracing::error!(error = %e, "artifact upload failed");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

fn refusal(reason: RejectReason, message: impl Into<String>) -> Response {
    (
        StatusCode::OK,
        Json(UploadGrant::Rejected {
            reason,
            message: message.into(),
        }),
    )
        .into_response()
}

/// A path label as stored: `/`-separated, with empty, `.` and `..` segments, control characters
/// and anything beyond the length caps removed. The label is for display and exports; it never
/// names a file on the server.
fn sanitize_path(raw: &str) -> Option<(String, String)> {
    let mut segments: Vec<String> = raw
        .replace('\\', "/")
        .split('/')
        .map(|s| {
            s.chars()
                .filter(|c| !c.is_control())
                .take(MAX_SEGMENT)
                .collect::<String>()
        })
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && s != "." && s != "..")
        .collect();
    while segments.join("/").len() > MAX_PATH && segments.len() > 1 {
        segments.remove(0);
    }
    let path: String = segments.join("/").chars().take(MAX_PATH).collect();
    let name = segments.last()?.clone();
    Some((path, name))
}

/// `image/png; charset=x` → `image/png`.
fn media_type(declared: &str) -> String {
    declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// What a request asks for, once it passed the policy.
struct Accepted {
    class: ArtifactClass,
    path: String,
    name: String,
    content_type: String,
}

/// The allowlist, the declared type, the size limit and the label: everything decided without
/// touching the database.
fn check(state: &AppState, req: &UploadRequest) -> Result<Accepted, Box<Response>> {
    let Some(class) = ArtifactClass::parse(&req.class) else {
        return Err(Box::new(refusal(
            RejectReason::ClassNotAllowed,
            format!("{:?} is not an accepted artifact class", req.class),
        )));
    };
    let content_type = media_type(&req.content_type);
    if !class.content_types().contains(&content_type.as_str()) {
        return Err(Box::new(refusal(
            RejectReason::ContentTypeNotAllowed,
            format!(
                "{} artifacts may be declared as {}",
                class.as_str(),
                class.content_types().join(", ")
            ),
        )));
    }
    let limit = state.config.artifacts.max_bytes.of(class);
    if req.size > limit {
        return Err(Box::new(refusal(
            RejectReason::TooLarge,
            format!("{} artifacts are limited to {limit} bytes", class.as_str()),
        )));
    }
    if Sha256Digest::from_hex(&req.sha256).is_none() {
        return Err(Box::new(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "sha256 must be 64 lowercase hex digits",
        )));
    }
    let Some((path, name)) = sanitize_path(&req.path) else {
        return Err(Box::new(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "path is empty",
        )));
    };
    Ok(Accepted {
        class,
        path,
        name,
        content_type,
    })
}

/// `POST /api/v1/agent/artifacts`: may I upload this file?
#[utoipa::path(post, path = "/api/v1/agent/artifacts", request_body = UploadRequest, responses(
    (status = 201, body = UploadGrant, description = "Send the bytes"),
    (status = 200, body = UploadGrant, description = "Nothing to send, or refused"),
    (status = 401, body = ApiError), (status = 409, body = ApiError, description =
        "The run or flow isn't known to the server yet: retry shortly"),
    (status = 422, body = ApiError)))]
pub(super) async fn request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UploadRequest>,
) -> Response {
    let publisher = match publisher(&state, &headers).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let accepted = match check(&state, &req) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let a = &state.config.artifacts;
    let ask = UploadAsk {
        project_id: publisher.project_id,
        credential: publisher.credential,
        agent_run_id: req.agent_run_id,
        flow_execution_id: req.flow_execution_id,
        step: req.step,
        class: accepted.class,
        path: &accepted.path,
        name: &accepted.name,
        content_type: &accepted.content_type,
        size: req.size,
        sha256: &req.sha256,
    };
    let quota = Quota {
        max_bytes: a.max_run_bytes,
        max_files: a.max_run_files,
    };
    let ttl = Duration::from_secs(a.grant_ttl_seconds);
    match state.store.request_upload(&ask, ttl, quota).await {
        Ok(Requested::Grant {
            artifact_id,
            expires_at,
        }) => {
            // A retry starts over: whatever an earlier attempt spooled is stale.
            let _ = tokio::fs::remove_file(spool_path(&state, artifact_id)).await;
            let base = format!("/api/v1/agent/artifacts/{artifact_id}");
            (
                StatusCode::CREATED,
                Json(UploadGrant::Upload {
                    artifact_id,
                    url: format!("{base}/content"),
                    complete_url: format!("{base}/complete"),
                    expires_at,
                    size: req.size,
                }),
            )
                .into_response()
        }
        Ok(Requested::Exists { artifact_id }) => {
            Json(UploadGrant::Exists { artifact_id }).into_response()
        }
        Ok(Requested::Rejected { reason, .. }) => {
            let reason = RejectReason::parse(&reason).unwrap_or(RejectReason::TypeNotAllowed);
            refusal(reason, "these bytes were refused before")
        }
        Ok(Requested::OverQuota) => refusal(
            RejectReason::RunQuotaExceeded,
            format!(
                "a run may store at most {} files and {} bytes",
                a.max_run_files, a.max_run_bytes
            ),
        ),
        Ok(Requested::UnknownRun) => error(
            StatusCode::CONFLICT,
            "unknown_run: the run has not connected yet",
        ),
        Ok(Requested::UnknownFlow) => error(
            StatusCode::CONFLICT,
            "unknown_flow: the flow has not reached the server yet",
        ),
        Err(e) => internal(&e),
    }
}

fn spool_path(state: &AppState, artifact: Uuid) -> PathBuf {
    state
        .config
        .artifacts
        .spool_dir
        .join(artifact.hyphenated().to_string())
}

/// Why receiving the bytes stopped.
enum Receive {
    TooMuch,
    TooLittle,
    Broken(io::Error),
}

/// Streams `body` into `file`, which must end up exactly `size` bytes long.
async fn receive(body: Body, file: &mut tokio::fs::File, size: u64) -> Result<(), Receive> {
    let mut received: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Receive::Broken(io::Error::other(e)))?;
        received = received.saturating_add(chunk.len() as u64);
        if received > size {
            return Err(Receive::TooMuch);
        }
        file.write_all(&chunk).await.map_err(Receive::Broken)?;
    }
    if received != size {
        return Err(Receive::TooLittle);
    }
    file.sync_all().await.map_err(Receive::Broken)
}

/// `PUT /api/v1/agent/artifacts/{artifact}/content`: the bytes, exactly once.
#[utoipa::path(put, path = "/api/v1/agent/artifacts/{artifact}/content",
    params(("artifact" = Uuid, Path, description = "Artifact ID from the grant")),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses((status = 204, description = "Received; call `complete`"),
        (status = 400, body = ApiError), (status = 404, body = ApiError),
        (status = 409, body = ApiError), (status = 410, body = ApiError),
        (status = 413, body = ApiError)))]
pub(super) async fn content(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(artifact): Path<Uuid>,
    body: Body,
) -> Response {
    let publisher = match publisher(&state, &headers).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let size = match state
        .store
        .claim_content(publisher.project_id, publisher.credential, artifact)
        .await
    {
        Ok(Claim::Claimed { size }) => size,
        Ok(Claim::NotFound) => return error(StatusCode::NOT_FOUND, "no such upload"),
        Ok(Claim::Expired) => {
            return error(StatusCode::GONE, "the grant has expired: request a new one");
        }
        Ok(Claim::AlreadyUsed) => {
            return error(StatusCode::CONFLICT, "the bytes were already received");
        }
        Err(e) => return internal(&e),
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let response = match declared {
        Some(n) if n == size => store_body(&state, artifact, body, size).await,
        Some(n) if n > size => Err(Box::new(error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("the grant is for {size} bytes"),
        ))),
        _ => Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            format!("Content-Length must be {size}"),
        ))),
    };
    match response {
        Ok(()) => match state.store.content_received(artifact).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => internal(&e),
        },
        Err(response) => {
            // The grant stays usable until it lapses: a cut connection can retry.
            let _ = tokio::fs::remove_file(spool_path(&state, artifact)).await;
            if let Err(e) = state.store.abort_content(artifact).await {
                tracing::warn!(error = %e, "releasing an upload claim");
            }
            *response
        }
    }
}

async fn store_body(
    state: &AppState,
    artifact: Uuid,
    body: Body,
    size: u64,
) -> Result<(), Box<Response>> {
    let path = spool_path(state, artifact);
    if let Some(dir) = path.parent()
        && let Err(e) = tokio::fs::create_dir_all(dir).await
    {
        return Err(Box::new(internal(&e)));
    }
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|e| Box::new(internal(&e)))?;
    match receive(body, &mut file, size).await {
        Ok(()) => Ok(()),
        Err(Receive::TooMuch) => Err(Box::new(error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("more than the {size} bytes the grant is for"),
        ))),
        Err(Receive::TooLittle) => Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            format!("fewer than the {size} bytes the grant is for"),
        ))),
        Err(Receive::Broken(e)) => {
            tracing::warn!(error = %e, "receiving an artifact body");
            Err(Box::new(error(
                StatusCode::BAD_REQUEST,
                "the body was cut short",
            )))
        }
    }
}

fn blob_class(class: ArtifactClass) -> BlobClass {
    match class {
        ArtifactClass::FailureScreenshot => BlobClass::Screenshot,
        ArtifactClass::Commands | ArtifactClass::MaestroLog | ArtifactClass::FailureHierarchy => {
            BlobClass::Log
        }
        ArtifactClass::Junit => BlobClass::Junit,
        ArtifactClass::HtmlReport => BlobClass::HtmlReport,
    }
}

/// The type the artifact is served as: what the bytes are, never what was declared.
fn served_type(kind: Kind) -> &'static str {
    match kind {
        Kind::Png => "image/png",
        Kind::Jpeg => "image/jpeg",
        Kind::Webp => "image/webp",
        Kind::Xml => "application/xml",
        Kind::Html => "text/html",
        Kind::Zip => "application/zip",
        Kind::TarZst => "application/zstd",
        Kind::Text => "text/plain",
    }
}

fn reason_of(e: &VerifyError) -> Option<RejectReason> {
    Some(match e {
        VerifyError::SizeMismatch { .. } => RejectReason::SizeMismatch,
        VerifyError::TooLarge { .. } => RejectReason::TooLarge,
        VerifyError::DigestMismatch { .. } => RejectReason::DigestMismatch,
        VerifyError::TypeNotAllowed { .. } => RejectReason::TypeNotAllowed,
        VerifyError::NotText => RejectReason::NotText,
        VerifyError::Archive(_) => RejectReason::UnsafeArchive,
        // Our disk failed, not the upload: not the uploader's fault.
        VerifyError::Io(_) => return None,
    })
}

fn result_json(artifact_id: Uuid, finished: Finished) -> Response {
    Json(match finished {
        Finished::Available => UploadResult::Available { artifact_id },
        Finished::Rejected(code) => UploadResult::Rejected {
            artifact_id,
            reason: RejectReason::parse(&code).unwrap_or(RejectReason::TypeNotAllowed),
            message: format!("refused: {code}"),
        },
    })
    .into_response()
}

/// `POST /api/v1/agent/artifacts/{artifact}/complete`: verify, store, and say what happened.
#[utoipa::path(post, path = "/api/v1/agent/artifacts/{artifact}/complete",
    params(("artifact" = Uuid, Path, description = "Artifact ID from the grant")),
    responses((status = 200, body = UploadResult), (status = 404, body = ApiError),
        (status = 409, body = ApiError, description = "The bytes have not arrived")))]
pub(super) async fn complete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(artifact): Path<Uuid>,
) -> Response {
    let publisher = match publisher(&state, &headers).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let pending = match state
        .store
        .pending_complete(publisher.project_id, publisher.credential, artifact)
        .await
    {
        Ok(Pending::Ready(p)) => p,
        Ok(Pending::Done(finished)) => return result_json(artifact, finished),
        Ok(Pending::NotFound) => return error(StatusCode::NOT_FOUND, "no such upload"),
        Ok(Pending::NotReceived) => {
            return error(StatusCode::CONFLICT, "the bytes have not arrived");
        }
        Err(e) => return internal(&e),
    };
    match finish(&state, publisher.project_id, &pending).await {
        Ok(finished) => result_json(artifact, finished),
        Err(Finish::Lost) => {
            // A concurrent `complete` finished it, or the spool was cleaned: say what is true.
            match state
                .store
                .pending_complete(publisher.project_id, publisher.credential, artifact)
                .await
            {
                Ok(Pending::Done(finished)) => result_json(artifact, finished),
                Ok(_) => error(
                    StatusCode::CONFLICT,
                    "the received bytes are gone: request a new grant",
                ),
                Err(e) => internal(&e),
            }
        }
        Err(Finish::Failed(e)) => internal(&e),
    }
}

enum Finish {
    /// The spool file isn't there.
    Lost,
    Failed(String),
}

async fn finish(state: &AppState, project: Uuid, p: &PendingArtifact) -> Result<Finished, Finish> {
    let path = spool_path(state, p.id);
    let failed = |e: &dyn std::fmt::Display| Finish::Failed(e.to_string());
    let digest = Sha256Digest::from_hex(&p.sha256).ok_or_else(|| failed(&"stored digest"))?;
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Finish::Lost),
        Err(e) => return Err(failed(&e)),
    };
    let limits = Limits {
        max_bytes: state.config.artifacts.max_bytes.of(p.class),
        ..Limits::default()
    };
    let (class, size) = (blob_class(p.class), p.size);
    let verified = tokio::task::spawn_blocking(move || {
        verify::verify(
            class,
            Declared {
                size,
                sha256: digest,
            },
            file,
            &limits,
        )
    })
    .await
    .map_err(|e| failed(&e))?;
    match verified {
        Ok(v) => {
            let blobs = state.blobs().await.map_err(|e| failed(&format!("{e:#}")))?;
            // Protect the blob from the garbage collector before writing it; a blob being
            // deleted right now is refused until it is gone (the agent retries `complete`).
            let reserved = state
                .store
                .reserve_blob(project, &p.sha256, v.size)
                .await
                .map_err(|e| failed(&e))?;
            if reserved == BlobReservation::BeingDeleted {
                return Err(failed(&"the blob is being garbage collected: retry"));
            }
            let body = tokio::fs::File::open(&path).await.map_err(|e| failed(&e))?;
            let stream: ByteStream<'_> = ReaderStream::new(body).boxed();
            blobs
                .put(&BlobKey::new(project, digest), v.size, stream)
                .await
                .map_err(|e| failed(&e))?;
            state
                .store
                .mark_available(project, p.id, served_type(v.kind))
                .await
                .map_err(|e| failed(&e))?;
            let _ = tokio::fs::remove_file(&path).await;
            Ok(Finished::Available)
        }
        Err(e) => {
            let Some(reason) = reason_of(&e) else {
                return Err(failed(&e));
            };
            tracing::info!(artifact_id = %p.id, reason = reason.as_str(), error = %e,
                "an artifact failed verification");
            state
                .store
                .mark_rejected(p.id, reason.as_str())
                .await
                .map_err(|e| failed(&e))?;
            let _ = tokio::fs::remove_file(&path).await;
            Ok(Finished::Rejected(reason.as_str().to_owned()))
        }
    }
}

/// Deletes spool files nobody will complete: not an open grant, not awaiting `complete`, and
/// older than two grant lifetimes.
pub(crate) async fn purge_spool(state: &AppState) {
    let dir = state.config.artifacts.spool_dir.clone();
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return;
    };
    let keep = match state.store.open_uploads().await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "listing open uploads");
            return;
        }
    };
    let grace = Duration::from_secs(state.config.artifacts.grant_ttl_seconds.saturating_mul(2));
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if is_stale(&path, &keep, grace).await {
            let _ = tokio::fs::remove_file(&path).await;
        }
    }
}

async fn is_stale(path: &FsPath, keep: &[Uuid], grace: Duration) -> bool {
    let Some(id) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| Uuid::parse_str(n).ok())
    else {
        return false;
    };
    if keep.contains(&id) {
        return false;
    }
    tokio::fs::metadata(path)
        .await
        .and_then(|m| m.modified())
        .is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() > grace)
}
