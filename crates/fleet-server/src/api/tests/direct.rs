// SPDX-License-Identifier: AGPL-3.0-only
//! Direct upload (PLAN §7.6) over the real router: a presigned grant, the bytes staged in the
//! object store, and the server's verification deciding what becomes visible.
//!
//! A store that stages in memory stands in for S3 where a test must inject bytes S3 would refuse
//! (a signed checksum makes S3 refuse them). The Garage tests at the end use the real thing.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::Mutex,
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use fleet_blob::{
    BlobError, BlobInfo, BlobKey, BlobRead, BlobStore, ByteStream, LocalFs, PutOutcome, Scope,
    Sha256Digest, StagingKey, testing::Garage,
};
use fleet_protocol::artifact::{RejectReason, UploadGrant, UploadRequest, UploadResult};
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use uuid::Uuid;

use super::{
    artifacts::{Dirs, PNG, artifact_rig, request, run_with_failed_flow, stored_blobs},
    rig::Rig,
};
use crate::config::{S3Settings, Secret, StorageBackend};

/// Blobs on local disk, and direct uploads staged in memory.
#[derive(Debug)]
struct Staging {
    disk: LocalFs,
    staged: Mutex<HashMap<Uuid, Vec<u8>>>,
    /// The headers each presigned URL was issued with.
    presigned: Mutex<Vec<(Uuid, BTreeMap<String, String>)>>,
    /// Make every presign fail, as a store that is unreachable at signing time would.
    refuse_presign: bool,
}

impl Staging {
    fn stage(&self, artifact: Uuid, bytes: &[u8]) {
        self.staged.lock().unwrap().insert(artifact, bytes.to_vec());
    }

    fn is_staged(&self, artifact: Uuid) -> bool {
        self.staged.lock().unwrap().contains_key(&artifact)
    }

    fn headers_of(&self, artifact: Uuid) -> BTreeMap<String, String> {
        self.presigned
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, _)| *id == artifact)
            .map(|(_, h)| h.clone())
            .expect("the grant was presigned")
    }
}

#[async_trait]
impl BlobStore for Staging {
    async fn put(
        &self,
        key: &BlobKey,
        size: u64,
        body: ByteStream<'_>,
    ) -> Result<PutOutcome, BlobError> {
        self.disk.put(key, size, body).await
    }

    async fn get(
        &self,
        key: &BlobKey,
        range: Option<std::ops::Range<u64>>,
    ) -> Result<BlobRead, BlobError> {
        self.disk.get(key, range).await
    }

    async fn head(&self, key: &BlobKey) -> Result<Option<BlobInfo>, BlobError> {
        self.disk.head(key).await
    }

    async fn delete(&self, key: &BlobKey) -> Result<(), BlobError> {
        self.disk.delete(key).await
    }

    fn list(&self, scope: Scope) -> BoxStream<'_, Result<BlobInfo, BlobError>> {
        self.disk.list(scope)
    }

    async fn presign_put(
        &self,
        staging: &StagingKey,
        _ttl: Duration,
        headers: &BTreeMap<String, String>,
    ) -> Result<Option<String>, BlobError> {
        if self.refuse_presign {
            return Err(BlobError::Config("the store refuses to sign".into()));
        }
        self.presigned
            .lock()
            .unwrap()
            .push((staging.artifact(), headers.clone()));
        Ok(Some(format!(
            "https://objects.test/uploads/{}",
            staging.artifact()
        )))
    }

    async fn staged(&self, staging: &StagingKey) -> Result<Option<BlobRead>, BlobError> {
        let Some(bytes) = self
            .staged
            .lock()
            .unwrap()
            .get(&staging.artifact())
            .cloned()
        else {
            return Ok(None);
        };
        let size = bytes.len() as u64;
        let chunks: Vec<io::Result<Bytes>> = vec![Ok(Bytes::from(bytes))];
        Ok(Some(BlobRead {
            size,
            range: 0..size,
            body: stream::iter(chunks).boxed(),
        }))
    }

    async fn delete_staged(&self, staging: &StagingKey) -> Result<(), BlobError> {
        self.staged.lock().unwrap().remove(&staging.artifact());
        Ok(())
    }
}

/// A server whose blob store can stage direct uploads in memory.
async fn staging_rig(refuse_presign: bool) -> (Rig, Dirs, std::sync::Arc<Staging>) {
    let (rig, dirs) = artifact_rig(|_| {}).await;
    let staging = std::sync::Arc::new(Staging {
        disk: LocalFs::new(&dirs.blobs).unwrap(),
        staged: Mutex::new(HashMap::new()),
        presigned: Mutex::new(Vec::new()),
        refuse_presign,
    });
    let store: std::sync::Arc<dyn BlobStore> = std::sync::Arc::<Staging>::clone(&staging);
    rig.state.install_blobs(store);
    (rig, dirs, staging)
}

fn direct(mut req: UploadRequest) -> UploadRequest {
    req.direct = true;
    req
}

/// The grant's artifact, presigned `PUT` URL and complete URL; panics on any other grant.
fn presigned(grant: UploadGrant) -> (Uuid, String, String) {
    match grant {
        UploadGrant::Presigned {
            artifact_id,
            url,
            complete_url,
            ..
        } => (artifact_id, url, complete_url),
        other => panic!("expected a presigned grant: {other:?}"),
    }
}

async fn complete(rig: &Rig, complete_url: &str) -> UploadResult {
    let res = rig.complete_as(&rig.key, complete_url).await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    res.json()
}

fn rejected(result: UploadResult) -> RejectReason {
    match result {
        UploadResult::Rejected { reason, .. } => reason,
        other @ UploadResult::Available { .. } => panic!("expected a rejection: {other:?}"),
    }
}

#[tokio::test]
async fn a_direct_request_gets_a_presigned_grant_bound_to_the_digest() {
    let (rig, _dirs, staging) = staging_rig(false).await;
    let run = run_with_failed_flow(&rig, "1001").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));

    let grant = rig.grant(&req).await;
    let UploadGrant::Presigned {
        artifact_id,
        url,
        headers,
        complete_url,
        size,
        ..
    } = grant
    else {
        panic!("expected a presigned grant");
    };
    assert_eq!(size, PNG.len() as u64);
    assert!(url.starts_with("https://objects.test/"), "{url}");
    assert_eq!(
        complete_url,
        format!("/api/v1/agent/artifacts/{artifact_id}/complete")
    );
    // The staging object is per artifact, never the content-addressed key.
    assert!(url.ends_with(&format!("/uploads/{artifact_id}")), "{url}");
    let expected = STANDARD.encode(Sha256Digest::of(PNG).as_bytes());
    assert_eq!(headers.get("x-amz-checksum-sha256"), Some(&expected));
    assert_eq!(headers.len(), 1, "{headers:?}");
    assert_eq!(staging.headers_of(artifact_id), headers);
}

#[tokio::test]
async fn a_relayed_request_still_gets_an_upload_grant() {
    let (rig, _dirs, _staging) = staging_rig(false).await;
    let run = run_with_failed_flow(&rig, "1002").await;
    let req = request(&run, "failure_screenshot", "s.png", "image/png", PNG);
    assert!(matches!(rig.grant(&req).await, UploadGrant::Upload { .. }));
}

#[tokio::test]
async fn a_store_that_cannot_presign_relays_the_upload_instead() {
    let (rig, dirs, _staging) = staging_rig(true).await;
    let run = run_with_failed_flow(&rig, "1003").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));

    // A failed presign is not an error: the grant is the relayed one, and it works.
    let UploadGrant::Upload {
        url, complete_url, ..
    } = rig.grant(&req).await
    else {
        panic!("expected a relayed grant when presigning fails");
    };
    let put = rig.put_as(&rig.key, &url, PNG).await;
    assert_eq!(put.status_code(), 204, "{}", put.text());
    assert!(matches!(
        complete(&rig, &complete_url).await,
        UploadResult::Available { .. }
    ));
    assert_eq!(stored_blobs(&dirs).await.len(), 1);
}

#[tokio::test]
async fn staged_bytes_are_verified_then_stored_and_the_staging_object_is_deleted() {
    let (rig, dirs, staging) = staging_rig(false).await;
    let run = run_with_failed_flow(&rig, "1004").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));
    let (artifact, _url, complete_url) = presigned(rig.grant(&req).await);
    staging.stage(artifact, PNG);

    let result = complete(&rig, &complete_url).await;
    assert!(
        matches!(result, UploadResult::Available { .. }),
        "{result:?}"
    );

    let project = rig.store.project_id("app").await.unwrap().unwrap();
    assert_eq!(
        stored_blobs(&dirs).await,
        [(project, Sha256Digest::of(PNG).to_string())]
    );
    assert!(
        !staging.is_staged(artifact),
        "the staging object is deleted on success"
    );
    let listed = rig.artifacts_of(run.flow).await;
    assert_eq!(listed["artifacts"][0]["state"], "available");
}

#[tokio::test]
async fn staged_bytes_that_fail_verification_never_become_visible() {
    let (rig, dirs, staging) = staging_rig(false).await;
    let run = run_with_failed_flow(&rig, "1005").await;
    // Declared as a screenshot, but text: the magic bytes refuse it.
    let text = b"definitely not a png, just text".to_vec();
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        &text,
    ));
    let (artifact, _url, complete_url) = presigned(rig.grant(&req).await);
    staging.stage(artifact, &text);

    let reason = rejected(complete(&rig, &complete_url).await);
    assert_eq!(reason, RejectReason::TypeNotAllowed);
    assert!(stored_blobs(&dirs).await.is_empty(), "no blob may exist");
    assert!(
        !staging.is_staged(artifact),
        "the staging object is deleted on failure"
    );
    let listed = rig.artifacts_of(run.flow).await;
    assert!(
        listed["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["state"] == "rejected")
    );
}

#[tokio::test]
async fn staged_bytes_with_a_different_digest_are_refused() {
    let (rig, dirs, staging) = staging_rig(false).await;
    let run = run_with_failed_flow(&rig, "1006").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));
    let (artifact, _url, complete_url) = presigned(rig.grant(&req).await);
    // Same length and still a PNG by its magic, but one byte differs from the declared digest.
    let mut other = PNG.to_vec();
    let last = other.len() - 1;
    other[last] ^= 0xff;
    staging.stage(artifact, &other);

    assert_eq!(
        rejected(complete(&rig, &complete_url).await),
        RejectReason::DigestMismatch
    );
    assert_eq!(stored_blobs(&dirs).await, Vec::new());
    assert!(!staging.is_staged(artifact));
}

#[tokio::test]
async fn staged_bytes_of_the_wrong_size_are_a_size_mismatch_both_ways() {
    for declared in [PNG.len() + 3, PNG.len() - 4] {
        let (rig, dirs, staging) = staging_rig(false).await;
        let run = run_with_failed_flow(&rig, &format!("size-{declared}")).await;
        let mut req = direct(request(
            &run,
            "failure_screenshot",
            "s.png",
            "image/png",
            PNG,
        ));
        req.size = declared as u64;
        let (artifact, _url, complete_url) = presigned(rig.grant(&req).await);
        staging.stage(artifact, PNG);

        assert_eq!(
            rejected(complete(&rig, &complete_url).await),
            RejectReason::SizeMismatch,
            "declared {declared}"
        );
        assert!(stored_blobs(&dirs).await.is_empty(), "declared {declared}");
        assert!(!staging.is_staged(artifact), "declared {declared}");
    }
}

#[tokio::test]
async fn nothing_staged_is_a_conflict_and_the_grant_stays_open() {
    let (rig, dirs, staging) = staging_rig(false).await;
    let run = run_with_failed_flow(&rig, "1007").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));
    let (artifact, _url, complete_url) = presigned(rig.grant(&req).await);

    assert_eq!(
        rig.complete_as(&rig.key, &complete_url).await.status_code(),
        409
    );
    // The bytes arrive late: the same grant completes.
    staging.stage(artifact, PNG);
    assert!(matches!(
        complete(&rig, &complete_url).await,
        UploadResult::Available { .. }
    ));
    assert_eq!(stored_blobs(&dirs).await.len(), 1);
}

#[tokio::test]
async fn a_second_completion_returns_the_outcome_and_never_reads_staging_again() {
    let (rig, dirs, staging) = staging_rig(false).await;
    let run = run_with_failed_flow(&rig, "1008").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));
    let (artifact, _url, complete_url) = presigned(rig.grant(&req).await);
    staging.stage(artifact, PNG);
    let first = complete(&rig, &complete_url).await;

    // The presigned URL is reusable until it expires: a re-staged object must change nothing.
    staging.stage(artifact, b"replacement bytes, never verified");
    let second = complete(&rig, &complete_url).await;
    assert_eq!(second, first);
    assert!(
        !staging.is_staged(artifact),
        "a stray staged object is removed"
    );
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    assert_eq!(
        stored_blobs(&dirs).await,
        [(project, Sha256Digest::of(PNG).to_string())]
    );
}

#[tokio::test]
async fn a_direct_grant_is_bound_to_its_credential() {
    let (rig, _dirs, _staging) = staging_rig(false).await;
    let run = run_with_failed_flow(&rig, "1009").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));
    let (_artifact, _url, complete_url) = presigned(rig.grant(&req).await);
    let res = rig.complete_as(super::rig::VIEWER, &complete_url).await;
    assert!(
        matches!(res.status_code().as_u16(), 401 | 404),
        "{}",
        res.status_code()
    );
}

/// Real S3: the reference Garage, with the server configured as the Compose file configures it.
fn garage_settings(garage: &Garage) -> impl FnOnce(&mut crate::config::Config) + '_ {
    move |c| {
        let s3 = garage.config();
        c.storage.backend = StorageBackend::S3;
        c.storage.s3 = S3Settings {
            endpoint: Some(s3.endpoint),
            region: s3.region,
            public_endpoint: None,
            bucket: Some(s3.bucket),
            access_key_id: Some(Secret::from(s3.access_key_id.as_str())),
            secret_access_key: Some(Secret::from(s3.secret_access_key.as_str())),
        };
    }
}

async fn read_blob(rig: &Rig, project: Uuid, digest: &str) -> Option<Vec<u8>> {
    let blobs = rig.state.blobs().await.unwrap();
    let key = BlobKey::new(project, Sha256Digest::from_hex(digest)?);
    let read = blobs.get(&key, None).await.ok()?;
    Some(
        read.body
            .try_fold(Vec::new(), |mut all, chunk| async move {
                all.extend_from_slice(&chunk);
                Ok(all)
            })
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn a_direct_upload_to_garage_is_verified_stored_and_its_staging_deleted() {
    let garage = Garage::start().unwrap_or_else(|e| panic!("{e}"));
    let _ready = garage.ready_store().await.unwrap();
    let (rig, _dirs) = artifact_rig(garage_settings(&garage)).await;
    let run = run_with_failed_flow(&rig, "1010").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));
    let grant = rig.grant(&req).await;
    let UploadGrant::Presigned {
        artifact_id,
        url,
        headers,
        complete_url,
        ..
    } = grant
    else {
        panic!("Garage presigns: {grant:?}");
    };

    // The agent's PUT: the signed header and no bearer credential at all.
    let mut put = reqwest::Client::new().put(&url).body(PNG.to_vec());
    for (name, value) in &headers {
        put = put.header(name, value);
    }
    let sent = put.send().await.unwrap();
    assert!(sent.status().is_success(), "{}", sent.status());

    let result = complete(&rig, &complete_url).await;
    assert!(
        matches!(result, UploadResult::Available { .. }),
        "{result:?}"
    );
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    assert_eq!(
        read_blob(&rig, project, &Sha256Digest::of(PNG).to_string())
            .await
            .as_deref(),
        Some(PNG)
    );
    let blobs = rig.state.blobs().await.unwrap();
    assert!(
        blobs
            .staged(&StagingKey::new(artifact_id))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn garage_refuses_bytes_that_do_not_match_the_signed_digest_and_nothing_is_stored() {
    let garage = Garage::start().unwrap_or_else(|e| panic!("{e}"));
    let _ready = garage.ready_store().await.unwrap();
    let (rig, _dirs) = artifact_rig(garage_settings(&garage)).await;
    let run = run_with_failed_flow(&rig, "1011").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));
    let grant = rig.grant(&req).await;
    let UploadGrant::Presigned {
        artifact_id,
        url,
        headers,
        complete_url,
        ..
    } = grant
    else {
        panic!("Garage presigns: {grant:?}");
    };
    let (name, value) = headers.iter().next().unwrap();
    let wrong = reqwest::Client::new()
        .put(&url)
        .header(name, value)
        .body(b"other bytes entirely".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 400);

    assert_eq!(
        rig.complete_as(&rig.key, &complete_url).await.status_code(),
        409
    );
    let blobs = rig.state.blobs().await.unwrap();
    assert!(
        blobs
            .staged(&StagingKey::new(artifact_id))
            .await
            .unwrap()
            .is_none()
    );
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    let digest = Sha256Digest::of(PNG);
    assert!(
        blobs
            .head(&BlobKey::new(project, digest))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn garage_keeps_a_reused_presigned_url_to_the_declared_bytes_and_a_finished_artifact_unchanged()
 {
    let garage = Garage::start().unwrap_or_else(|e| panic!("{e}"));
    let _ready = garage.ready_store().await.unwrap();
    let (rig, _dirs) = artifact_rig(garage_settings(&garage)).await;
    let run = run_with_failed_flow(&rig, "1012").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));
    let grant = rig.grant(&req).await;
    let UploadGrant::Presigned {
        artifact_id,
        url,
        headers,
        complete_url,
        ..
    } = grant
    else {
        panic!("Garage presigns");
    };
    let client = reqwest::Client::new();
    let (name, value) = headers.iter().next().unwrap();
    assert!(
        client
            .put(&url)
            .header(name, value)
            .body(PNG.to_vec())
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(matches!(
        complete(&rig, &complete_url).await,
        UploadResult::Available { .. }
    ));

    // Reuse: the signed digest admits only the declared bytes, so other bytes are refused...
    let other = client
        .put(&url)
        .header(name, value)
        .body(b"replacement".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 400);
    // ...and the same bytes may be staged again, which changes nothing: completing returns the
    // recorded outcome and the stored blob is untouched.
    let again = client
        .put(&url)
        .header(name, value)
        .body(PNG.to_vec())
        .send()
        .await
        .unwrap();
    assert!(again.status().is_success());
    assert!(matches!(
        complete(&rig, &complete_url).await,
        UploadResult::Available { .. }
    ));
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    assert_eq!(
        read_blob(&rig, project, &Sha256Digest::of(PNG).to_string())
            .await
            .as_deref(),
        Some(PNG)
    );
    // The completion that found the re-staged object removed it.
    let blobs = rig.state.blobs().await.unwrap();
    assert!(
        blobs
            .staged(&StagingKey::new(artifact_id))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_purge_removes_a_direct_upload_nobody_completed_once_its_grace_has_passed() {
    let garage = Garage::start().unwrap_or_else(|e| panic!("{e}"));
    let _ready = garage.ready_store().await.unwrap();
    // A one-second grant: the grace period (two grant lifetimes) is two seconds of wall clock.
    let (rig, _dirs) = artifact_rig(|c| {
        garage_settings(&garage)(c);
        c.artifacts.grant_ttl_seconds = 1;
    })
    .await;
    let run = run_with_failed_flow(&rig, "1013").await;
    let req = direct(request(
        &run,
        "failure_screenshot",
        "s.png",
        "image/png",
        PNG,
    ));
    let grant = rig.grant(&req).await;
    let UploadGrant::Presigned {
        artifact_id: artifact,
        url,
        headers,
        ..
    } = grant
    else {
        panic!("Garage presigns: {grant:?}");
    };
    let (name, value) = headers.iter().next().unwrap();
    let sent = reqwest::Client::new()
        .put(&url)
        .header(name, value)
        .body(PNG.to_vec())
        .send()
        .await;
    assert!(sent.unwrap().status().is_success());
    let blobs = rig.state.blobs().await.unwrap();
    assert!(
        blobs
            .staged(&StagingKey::new(artifact))
            .await
            .unwrap()
            .is_some()
    );

    // Within the grace the object is kept; past it, with no open grant, the purge removes it.
    crate::artifacts::purge_spool(&rig.state).await;
    assert!(
        blobs
            .staged(&StagingKey::new(artifact))
            .await
            .unwrap()
            .is_some()
    );
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    crate::artifacts::purge_spool(&rig.state).await;
    assert!(
        blobs
            .staged(&StagingKey::new(artifact))
            .await
            .unwrap()
            .is_none()
    );
}
