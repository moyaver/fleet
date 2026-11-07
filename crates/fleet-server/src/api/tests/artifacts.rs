// SPDX-License-Identifier: AGPL-3.0-only
//! The artifact upload protocol over HTTP (seam S6): the real router on a fresh Postgres database
//! and a real blob directory, driven in-process with axum-test the way an agent drives it:
//! `upload_request` → grant → `PUT` → `upload_complete`. Runs are opened through the real ingest
//! path. Expected values come from PLAN §6.6/§7.6 and worked literals (a 1×1 PNG, known digests).

use std::path::PathBuf;

use axum_test::TestResponse;
use fleet_blob::{BlobKey, BlobStore, LocalFs, Scope, Sha256Digest};
use fleet_protocol::{
    artifact::{RejectReason, UploadGrant, UploadRequest, UploadResult},
    event::{FlowOutcome, StepStatus},
};
use futures_util::TryStreamExt;
use serde_json::Value;
use uuid::Uuid;

use super::rig::{Agent, Rig, rig_with};
use crate::config::Config;

/// A 1×1 transparent PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

fn sha256(bytes: &[u8]) -> String {
    Sha256Digest::of(bytes).to_string()
}

/// Test directories: the blob store and the upload spool.
pub(super) struct Dirs {
    pub(super) blobs: PathBuf,
    pub(super) spool: PathBuf,
}

pub(super) fn dirs() -> Dirs {
    let root = std::env::temp_dir().join(format!("fleet-artifacts-{}", Uuid::now_v7()));
    Dirs {
        blobs: root.join("blobs"),
        spool: root.join("spool"),
    }
}

pub(super) async fn artifact_rig(tweak: impl FnOnce(&mut Config)) -> (Rig, Dirs) {
    let dirs = dirs();
    let (blobs, spool) = (dirs.blobs.clone(), dirs.spool.clone());
    let rig = rig_with(|c| {
        c.storage.path = blobs;
        c.artifacts.spool_dir = spool;
        tweak(c);
    })
    .await;
    (rig, dirs)
}

/// An agent run that is known to the server, with one failed flow.
pub(super) struct Run {
    pub(super) run: Uuid,
    pub(super) flow: Uuid,
}

pub(super) async fn run_with_failed_flow(rig: &Rig, ci_run: &str) -> Run {
    let mut agent = Agent::new(ci_run, "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["checkout"]);
    let flow = agent.flow_started(&shard, "checkout", 0);
    agent.step(&flow, 0, "Assert visible \"Done\"", StepStatus::Failed);
    agent.flow_ended(&flow, FlowOutcome::Failed, Some("Element not found: Done"));
    rig.publish(&mut agent, vec![]).await;
    Run {
        run: agent.id,
        flow: flow.subject.flow_execution_id.unwrap(),
    }
}

pub(super) fn request(
    run: &Run,
    class: &str,
    path: &str,
    ctype: &str,
    bytes: &[u8],
) -> UploadRequest {
    UploadRequest {
        agent_run_id: run.run,
        flow_execution_id: Some(run.flow),
        step: None,
        class: class.into(),
        path: path.into(),
        content_type: ctype.into(),
        size: bytes.len() as u64,
        sha256: sha256(bytes),
    }
}

impl Rig {
    pub(super) async fn upload_request_as(&self, key: &str, req: &UploadRequest) -> TestResponse {
        self.app
            .post("/api/v1/agent/artifacts")
            .authorization_bearer(key)
            .json(req)
            .await
    }

    pub(super) async fn grant(&self, req: &UploadRequest) -> UploadGrant {
        let res = self.upload_request_as(&self.key, req).await;
        assert!(res.status_code().is_success(), "{}", res.text());
        res.json()
    }

    pub(super) async fn put_as(&self, key: &str, url: &str, bytes: &[u8]) -> TestResponse {
        self.app
            .put(url)
            .authorization_bearer(key)
            .bytes(bytes.to_vec().into())
            .await
    }

    pub(super) async fn complete_as(&self, key: &str, url: &str) -> TestResponse {
        self.app.post(url).authorization_bearer(key).await
    }

    /// The whole protocol for one file; returns the final result.
    pub(super) async fn upload(&self, req: &UploadRequest, bytes: &[u8]) -> UploadResult {
        let UploadGrant::Upload {
            url, complete_url, ..
        } = self.grant(req).await
        else {
            panic!("expected an upload grant");
        };
        let put = self.put_as(&self.key, &url, bytes).await;
        assert_eq!(put.status_code(), 204, "{}", put.text());
        let done = self.complete_as(&self.key, &complete_url).await;
        assert_eq!(done.status_code(), 200, "{}", done.text());
        done.json()
    }

    pub(super) async fn artifacts_of(&self, flow: Uuid) -> Value {
        self.json(&format!("/api/v1/projects/app/flows/{flow}/artifacts"))
            .await
    }
}

pub(super) async fn stored_blobs(dirs: &Dirs) -> Vec<(Uuid, String)> {
    let store = LocalFs::new(&dirs.blobs).unwrap();
    let mut found: Vec<_> = store
        .list(Scope::All)
        .map_ok(|b| (b.key.project(), b.key.digest().to_string()))
        .try_collect()
        .await
        .unwrap();
    found.sort();
    found
}

#[tokio::test]
async fn an_agent_uploads_a_failure_screenshot_and_the_flow_lists_it() {
    let (rig, dirs) = artifact_rig(|_| {}).await;
    let run = run_with_failed_flow(&rig, "900").await;
    let req = request(
        &run,
        "failure_screenshot",
        "checkout/screenshots/step-002-assert.png",
        "image/png",
        PNG,
    );

    let result = rig.upload(&req, PNG).await;

    let UploadResult::Available { artifact_id } = result else {
        panic!("expected the screenshot to be available: {result:?}");
    };
    let listed = rig.artifacts_of(run.flow).await;
    let items = listed["artifacts"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{listed}");
    let a = &items[0];
    assert_eq!(a["id"], artifact_id.to_string());
    assert_eq!(a["class"], "failure_screenshot");
    assert_eq!(a["name"], "step-002-assert.png");
    assert_eq!(a["path"], "checkout/screenshots/step-002-assert.png");
    assert_eq!(a["content_type"], "image/png");
    assert_eq!(a["size_bytes"], 67);
    assert_eq!(a["sha256"], sha256(PNG));
    assert_eq!(a["state"], "available");
    // Evidence of a flow that had already failed starts with failure retention (research 05 §12.3).
    assert_eq!(a["retention_class"], "failure_evidence");

    // The flow's own view names it too.
    let detail = rig
        .json(&format!("/api/v1/projects/app/flows/{}", run.flow))
        .await;
    assert_eq!(detail["artifacts"][0]["id"], artifact_id.to_string());

    // Content-addressed, per project: exactly one blob, keyed by project and digest.
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    assert_eq!(stored_blobs(&dirs).await, [(project, sha256(PNG))]);
    let blob = LocalFs::new(&dirs.blobs)
        .unwrap()
        .head(&BlobKey::new(project, Sha256Digest::of(PNG)))
        .await
        .unwrap();
    assert_eq!(blob.map(|b| b.size), Some(67));
}

/// A failing step's UI hierarchy is an allowlisted JSON class of its own: its own content type,
/// its own size limit, and it is stored and listed like a screenshot (issue #74).
#[tokio::test]
async fn an_agent_uploads_a_failure_hierarchy_within_its_own_limit() {
    // 128 bytes: the JSON below fits, the 67-byte PNG fits, a padded copy does not.
    let (rig, _) = artifact_rig(|c| c.artifacts.max_bytes.failure_hierarchy = 128).await;
    let run = run_with_failed_flow(&rig, "940").await;
    let json = br#"{"children":[{"attributes":{"text":"[REDACTED]"}}]}"#;
    let path = "checkout/screen-hierarchy/step-001-assertVisible-Done.json";

    let result = rig
        .upload(
            &request(&run, "failure_hierarchy", path, "application/json", json),
            json,
        )
        .await;
    assert!(
        matches!(result, UploadResult::Available { .. }),
        "{result:?}"
    );
    let listed = rig.artifacts_of(run.flow).await;
    let a = &listed["artifacts"][0];
    assert_eq!(a["class"], "failure_hierarchy", "{listed}");
    assert_eq!(a["name"], "step-001-assertVisible-Done.json");
    // Served as the text it verifies as, like commands.json, not as the declared type.
    assert_eq!(a["content_type"], "text/plain");
    assert_eq!(a["state"], "available");
    assert_eq!(a["retention_class"], "failure_evidence");

    // Declared as a picture, or as a screenshot's bytes, it is refused and nothing is kept.
    let g = rig
        .grant(&request(
            &run,
            "failure_hierarchy",
            "a.json",
            "image/png",
            json,
        ))
        .await;
    assert_eq!(refused(g), RejectReason::ContentTypeNotAllowed);
    // The bytes are checked once they arrive: a screenshot's bytes are not a hierarchy.
    let result = rig
        .upload(
            &request(&run, "failure_hierarchy", "b.json", "application/json", PNG),
            PNG,
        )
        .await;
    assert!(
        matches!(
            result,
            UploadResult::Rejected {
                reason: RejectReason::TypeNotAllowed,
                ..
            }
        ),
        "{result:?}"
    );
    let big = [json.as_slice(), &[b' '; 128]].concat();
    let g = rig
        .grant(&request(
            &run,
            "failure_hierarchy",
            "c.json",
            "application/json",
            &big,
        ))
        .await;
    assert_eq!(refused(g), RejectReason::TooLarge);
    let listed = rig.artifacts_of(run.flow).await;
    let available = listed["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["state"] == "available")
        .count();
    assert_eq!(available, 1, "{listed}");
}

fn refused(g: UploadGrant) -> RejectReason {
    match g {
        UploadGrant::Rejected { reason, .. } => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn asking_again_is_idempotent_and_the_same_bytes_are_stored_once_per_project() {
    let (rig, dirs) = artifact_rig(|_| {}).await;
    let first = run_with_failed_flow(&rig, "910").await;
    let req = request(&first, "failure_screenshot", "a/shot.png", "image/png", PNG);
    let UploadResult::Available { artifact_id } = rig.upload(&req, PNG).await else {
        panic!("first upload");
    };

    // Same run, same class, same bytes: the same artifact, nothing to send.
    assert_eq!(rig.grant(&req).await, UploadGrant::Exists { artifact_id });
    assert_eq!(
        rig.artifacts_of(first.flow).await["artifacts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Another run of the same project: its own artifact, but the project already holds the
    // bytes, so there is nothing to send (deduplication) and one blob on disk.
    let second = run_with_failed_flow(&rig, "911").await;
    let again = request(
        &second,
        "failure_screenshot",
        "b/other-name.png",
        "image/png",
        PNG,
    );
    let UploadGrant::Exists { artifact_id: other } = rig.grant(&again).await else {
        panic!("a dedup hit should say `exists`");
    };
    assert_ne!(other, artifact_id, "each run has its own artifact row");
    let listed = rig.artifacts_of(second.flow).await;
    assert_eq!(listed["artifacts"][0]["id"], other.to_string());
    assert_eq!(listed["artifacts"][0]["name"], "other-name.png");
    assert_eq!(stored_blobs(&dirs).await.len(), 1);

    // Another project never learns that these bytes exist elsewhere (no cross-tenant probing):
    // it is asked to upload them, and gets its own blob.
    let other_project = rig.store.project_id("other").await.unwrap().unwrap();
    let (_, other_key) = rig
        .store
        .create_api_key(other_project, "ci", fleet_store::auth::TrustLevel::Trusted)
        .await
        .unwrap();
    let mut agent = Agent::new("912", "build", "runner-b");
    agent.started();
    rig.publish_as(other_key.expose(), &mut agent, vec![]).await;
    let hello = agent.id;
    let foreign = UploadRequest {
        agent_run_id: hello,
        flow_execution_id: None,
        step: None,
        class: "failure_screenshot".into(),
        path: "x.png".into(),
        content_type: "image/png".into(),
        size: PNG.len() as u64,
        sha256: sha256(PNG),
    };
    let res = rig.upload_request_as(other_key.expose(), &foreign).await;
    let grant: UploadGrant = res.json();
    assert!(matches!(grant, UploadGrant::Upload { .. }), "{grant:?}");
}

#[tokio::test]
async fn only_allowlisted_classes_types_and_sizes_get_a_grant() {
    let (rig, dirs) = artifact_rig(|c| c.artifacts.max_bytes.failure_screenshot = 50).await;
    let run = run_with_failed_flow(&rig, "920").await;
    let ask = |class: &str, ctype: &str, bytes: &[u8]| request(&run, class, "f", ctype, bytes);

    // A class that is not on the allowlist (research 06: never "everything in the output dir").
    for class in ["video", "env_dump", "", "FAILURE_SCREENSHOT"] {
        let g = rig.grant(&ask(class, "image/png", PNG)).await;
        assert_eq!(refused(g), RejectReason::ClassNotAllowed, "{class:?}");
    }
    // A content type the class does not accept: an HTML page declared as a screenshot.
    let g = rig
        .grant(&ask("failure_screenshot", "text/html", b"<html>"))
        .await;
    assert_eq!(refused(g), RejectReason::ContentTypeNotAllowed);
    // The declared type may carry parameters.
    let g = rig
        .grant(&ask("maestro_log", "text/plain; charset=utf-8", b"log"))
        .await;
    assert!(matches!(g, UploadGrant::Upload { .. }), "{g:?}");
    // The per-class size limit from the configuration: 50 bytes here, the PNG is 67.
    let g = rig
        .grant(&ask("failure_screenshot", "image/png", PNG))
        .await;
    assert_eq!(refused(g), RejectReason::TooLarge);

    // Nothing was stored or even asked of the blob store.
    assert_eq!(stored_blobs(&dirs).await.len(), 0);
    assert_eq!(
        rig.artifacts_of(run.flow).await["artifacts"],
        serde_json::json!([])
    );
}

/// The excerpt of a failed flow's device log is an allowlisted text class with its own size limit,
/// and it lists on the flow as failure evidence (issue #75).
#[tokio::test]
async fn a_device_logcat_excerpt_is_text_within_its_own_limit_and_lists_on_its_flow() {
    let (rig, _dirs) = artifact_rig(|c| c.artifacts.max_bytes.device_logcat = 64).await;
    let run = run_with_failed_flow(&rig, "921").await;
    let text: &[u8] = b"11-07 01:41:44.553 I/adbd: adbd service requested\n";
    let ask = |ctype: &str, bytes: &[u8]| {
        request(
            &run,
            "device_logcat",
            "checkout/logs/device-logcat.txt",
            ctype,
            bytes,
        )
    };

    let g = rig.grant(&ask("text/html", text)).await;
    assert_eq!(refused(g), RejectReason::ContentTypeNotAllowed);
    let g = rig.grant(&ask("text/plain", &[b'a'; 65])).await;
    assert_eq!(refused(g), RejectReason::TooLarge, "the class's own limit");

    let result = rig.upload(&ask("text/plain", text), text).await;
    assert!(
        matches!(result, UploadResult::Available { .. }),
        "{result:?}"
    );
    let listed = rig.artifacts_of(run.flow).await;
    let excerpt = listed["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["class"] == "device_logcat")
        .unwrap_or_else(|| panic!("the excerpt is listed on its flow: {listed}"));
    assert_eq!(excerpt["name"], "device-logcat.txt");
    assert_eq!(excerpt["state"], "available");
    assert_eq!(excerpt["retention_class"], "failure_evidence");
}

fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;
    let mut out = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut out);
    for (name, bytes) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
    out.into_inner()
}

#[tokio::test]
async fn bytes_that_fail_verification_are_rejected_with_a_reason_and_nothing_is_kept() {
    let (rig, dirs) = artifact_rig(|c| {
        c.artifacts.max_bytes.html_report = 10 * 1024 * 1024;
    })
    .await;
    let run = run_with_failed_flow(&rig, "930").await;
    let bomb = zip_of(&[("report/index.html", &vec![b'a'; 5 * 1024 * 1024])]);
    let traversal = zip_of(&[("../../evil.html", b"<html>")]);
    let with_nul = b"line one\0line two".to_vec();
    let cases: [(&str, &str, &str, Vec<u8>, RejectReason); 5] = [
        // The class says screenshot, the bytes say zip: magic bytes win over names and headers.
        (
            "failure_screenshot",
            "image/png",
            "a.png",
            zip_of(&[("a", b"b")]),
            RejectReason::TypeNotAllowed,
        ),
        // Compression ratio far beyond 100:1 (research 06 T11).
        (
            "html_report",
            "application/zip",
            "bomb.zip",
            bomb,
            RejectReason::UnsafeArchive,
        ),
        // An entry that climbs out of the extraction directory.
        (
            "html_report",
            "application/zip",
            "t.zip",
            traversal,
            RejectReason::UnsafeArchive,
        ),
        // A log must be UTF-8 text without NUL bytes.
        (
            "maestro_log",
            "text/plain",
            "maestro.log",
            with_nul,
            RejectReason::NotText,
        ),
        (
            "junit",
            "application/xml",
            "report.xml",
            PNG.to_vec(),
            RejectReason::TypeNotAllowed,
        ),
    ];
    for (class, ctype, path, bytes, want) in cases {
        let req = request(&run, class, path, ctype, &bytes);
        let UploadResult::Rejected { reason, .. } = rig.upload(&req, &bytes).await else {
            panic!("{path} should have been rejected");
        };
        assert_eq!(reason, want, "{path}");
        // The refusal is remembered: asking again is refused the same way, without an upload.
        assert_eq!(refused(rig.grant(&req).await), want, "{path} asked again");
    }
    // A digest that is not the content's.
    let mut lying = request(&run, "failure_screenshot", "x.png", "image/png", PNG);
    lying.sha256 = sha256(b"something else");
    let UploadResult::Rejected { reason, .. } = rig.upload(&lying, PNG).await else {
        panic!("a wrong digest should be rejected");
    };
    assert_eq!(reason, RejectReason::DigestMismatch);

    assert!(
        stored_blobs(&dirs).await.is_empty(),
        "no refused byte is ever stored"
    );
    let listed = rig.artifacts_of(run.flow).await;
    let states: Vec<_> = listed["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["state"].as_str().unwrap(),
                a["reject_reason"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(states.len(), 6, "{listed}");
    assert!(states.iter().all(|(s, _)| *s == "rejected"));
    assert!(
        listed["artifacts"][0]["link"].is_null(),
        "a refused artifact has no link"
    );
    let spooled = std::fs::read_dir(&dirs.spool).map_or(0, Iterator::count);
    assert_eq!(spooled, 0, "refused bytes leave the spool");
}

#[tokio::test]
async fn labels_are_sanitised_and_never_name_where_bytes_are_stored() {
    let (rig, _dirs) = artifact_rig(|_| {}).await;
    let run = run_with_failed_flow(&rig, "940").await;
    let req = request(
        &run,
        "failure_screenshot",
        "../../etc/./passwd\\x/shot\n.png",
        "image/png",
        PNG,
    );
    rig.upload(&req, PNG).await;

    let listed = rig.artifacts_of(run.flow).await;
    assert_eq!(listed["artifacts"][0]["path"], "etc/passwd/x/shot.png");
    assert_eq!(listed["artifacts"][0]["name"], "shot.png");
    let empty = request(&run, "failure_screenshot", "../..", "image/png", PNG);
    let res = rig.upload_request_as(&rig.key, &empty).await;
    assert_eq!(res.status_code(), 422);
}

async fn other_key(rig: &Rig, project: &str) -> String {
    let id = rig.store.project_id(project).await.unwrap().unwrap();
    let (_, key) = rig
        .store
        .create_api_key(id, "ci", fleet_store::auth::TrustLevel::Trusted)
        .await
        .unwrap();
    key.expose().to_owned()
}

fn urls(g: UploadGrant) -> (String, String) {
    match g {
        UploadGrant::Upload {
            url, complete_url, ..
        } => (url, complete_url),
        other => panic!("expected an upload grant, got {other:?}"),
    }
}

#[tokio::test]
async fn a_grant_belongs_to_one_credential_and_takes_the_bytes_once() {
    let (rig, dirs) = artifact_rig(|_| {}).await;
    let run = run_with_failed_flow(&rig, "950").await;
    let req = request(&run, "failure_screenshot", "s.png", "image/png", PNG);
    let (put, complete) = urls(rig.grant(&req).await);

    // Another project's key, a viewer token and no credential at all can neither send bytes to
    // this grant nor finish it, and learn nothing: it looks like there is no such upload.
    let intruder = other_key(&rig, "other").await;
    for (who, key) in [
        ("another project", intruder.as_str()),
        ("a read-only token", super::rig::VIEWER),
    ] {
        let res = rig.put_as(key, &put, PNG).await;
        assert!(
            matches!(res.status_code().as_u16(), 401 | 404),
            "{who}: {}",
            res.status_code()
        );
        let res = rig.complete_as(key, &complete).await;
        assert!(
            matches!(res.status_code().as_u16(), 401 | 404),
            "{who}: {}",
            res.status_code()
        );
    }
    assert_eq!(
        rig.app
            .put(&put)
            .bytes(PNG.to_vec().into())
            .await
            .status_code(),
        401
    );

    // Completing before the bytes arrived is refused.
    assert_eq!(
        rig.complete_as(&rig.key, &complete).await.status_code(),
        409
    );

    // The wrong amount of bytes is refused, and the grant stays usable.
    assert_eq!(
        rig.put_as(&rig.key, &put, PNG.get(..10).unwrap())
            .await
            .status_code(),
        400
    );
    let too_much = [PNG, b"extra"].concat();
    assert_eq!(
        rig.put_as(&rig.key, &put, &too_much).await.status_code(),
        413
    );
    assert_eq!(rig.put_as(&rig.key, &put, PNG).await.status_code(), 204);

    // Single use: the bytes were taken, a second PUT is refused even with the right content.
    assert_eq!(rig.put_as(&rig.key, &put, PNG).await.status_code(), 409);
    let done: UploadResult = rig.complete_as(&rig.key, &complete).await.json();
    assert!(matches!(done, UploadResult::Available { .. }));
    // Completing twice says the same thing.
    let again: UploadResult = rig.complete_as(&rig.key, &complete).await.json();
    assert_eq!(again, done);
    assert_eq!(stored_blobs(&dirs).await.len(), 1);
}

#[tokio::test]
async fn an_expired_grant_takes_no_bytes_and_can_be_asked_for_again() {
    // A zero lifetime: the grant has lapsed by the time the bytes arrive.
    let (rig, dirs) = artifact_rig(|c| c.artifacts.grant_ttl_seconds = 0).await;
    let run = run_with_failed_flow(&rig, "960").await;
    let req = request(&run, "failure_screenshot", "s.png", "image/png", PNG);
    let (put, _) = urls(rig.grant(&req).await);

    assert_eq!(rig.put_as(&rig.key, &put, PNG).await.status_code(), 410);

    // Asking again is the way to recover; it names the same artifact.
    let (again, _) = urls(rig.grant(&req).await);
    assert_eq!(again, put);
    assert_eq!(stored_blobs(&dirs).await.len(), 0);
}

#[tokio::test]
async fn a_request_must_name_a_run_and_flow_of_the_callers_own_project() {
    let (rig, _dirs) = artifact_rig(|_| {}).await;
    let run = run_with_failed_flow(&rig, "970").await;
    let intruder = other_key(&rig, "other").await;
    let req = request(&run, "failure_screenshot", "s.png", "image/png", PNG);

    // Another project cannot attach artifacts to this project's run: it does not exist for it.
    let res = rig.upload_request_as(&intruder, &req).await;
    assert_eq!(res.status_code(), 409);
    assert!(res.text().contains("unknown_run"), "{}", res.text());

    // An unknown run, and a flow that is not (yet) known: retryable conflicts, not refusals.
    let mut unknown_run = req.clone();
    unknown_run.agent_run_id = Uuid::now_v7();
    assert_eq!(
        rig.upload_request_as(&rig.key, &unknown_run)
            .await
            .status_code(),
        409
    );
    let mut unknown_flow = req.clone();
    unknown_flow.flow_execution_id = Some(Uuid::now_v7());
    let res = rig.upload_request_as(&rig.key, &unknown_flow).await;
    assert_eq!(res.status_code(), 409);
    assert!(res.text().contains("unknown_flow"), "{}", res.text());

    // Read-only access cannot upload, and neither can an unauthenticated caller.
    let res = rig.upload_request_as(super::rig::VIEWER, &req).await;
    assert_eq!(res.status_code(), 401);
    let res = rig.app.post("/api/v1/agent/artifacts").json(&req).await;
    assert_eq!(res.status_code(), 401);
}

#[tokio::test]
async fn a_run_cannot_store_more_files_or_bytes_than_its_quota() {
    let (rig, _dirs) = artifact_rig(|c| {
        c.artifacts.max_run_files = 2;
        c.artifacts.max_run_bytes = 100;
    })
    .await;
    let run = run_with_failed_flow(&rig, "980").await;
    let log = |n: u8| request(&run, "maestro_log", "m.log", "text/plain", &[n; 30]);

    assert!(matches!(
        rig.grant(&log(1)).await,
        UploadGrant::Upload { .. }
    ));
    assert!(matches!(
        rig.grant(&log(2)).await,
        UploadGrant::Upload { .. }
    ));
    assert_eq!(
        refused(rig.grant(&log(3)).await),
        RejectReason::RunQuotaExceeded,
        "two files"
    );
    // Asking again for one it already has is still fine: idempotent, not a new file.
    assert!(matches!(
        rig.grant(&log(1)).await,
        UploadGrant::Upload { .. }
    ));

    let (rig, _dirs) = artifact_rig(|c| c.artifacts.max_run_bytes = 100).await;
    let run = run_with_failed_flow(&rig, "981").await;
    let big = |n: u8| request(&run, "maestro_log", "m.log", "text/plain", &[n; 60]);
    assert!(matches!(
        rig.grant(&big(1)).await,
        UploadGrant::Upload { .. }
    ));
    assert_eq!(
        refused(rig.grant(&big(2)).await),
        RejectReason::RunQuotaExceeded,
        "120 > 100 bytes"
    );
}

#[tokio::test]
async fn a_flow_lists_its_own_evidence_and_its_runs_report_and_log_but_not_another_runs() {
    let (rig, _dirs) = artifact_rig(|_| {}).await;
    let run = run_with_failed_flow(&rig, "990").await;
    let other = run_with_failed_flow(&rig, "991").await;
    let report = b"<?xml version=\"1.0\"?><testsuites/>";
    let mut junit = request(&run, "junit", "report.xml", "application/xml", report);
    junit.flow_execution_id = None;
    rig.upload(&junit, report).await;
    rig.upload(
        &request(&run, "failure_screenshot", "s.png", "image/png", PNG),
        PNG,
    )
    .await;
    let mut elsewhere = request(&other, "maestro_log", "maestro.log", "text/plain", b"log");
    elsewhere.flow_execution_id = None;
    rig.upload(&elsewhere, b"log").await;

    let listed = rig.artifacts_of(run.flow).await;
    let seen: Vec<(&str, bool)> = listed["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["class"].as_str().unwrap(),
                a["flow_execution_id"].is_null(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [("junit", true), ("failure_screenshot", false)],
        "run-level files say so with a null flow; another run's log is not here"
    );
}
