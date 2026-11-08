// SPDX-License-Identifier: AGPL-3.0-only
//! Artifact expiry and blob garbage collection (PLAN §7.6, §7.7; research 05 §12.4).
//!
//! Artifacts and blobs have separate lives. An artifact is one run's claim on content; a blob is
//! the content, stored once per project however many artifacts name it. So:
//!
//! 1. [`Store::expire_artifacts`] retires artifacts past their retention (never those on legal
//!    hold) and abandoned upload requests. When that leaves a blob with no artifact in
//!    `declared`, `uploading` or `available`, the blob's grace period starts.
//! 2. [`Store::claim_garbage_blobs`] hands out blobs whose grace period has run out, after
//!    checking again that nothing refers to them. The server deletes each object, then calls
//!    [`Store::forget_blob`]. Counting references with `NOT EXISTS` over the indexed
//!    `artifacts(project_id, sha256)` avoids hot reference counters.
//! 3. [`Store::reserve_blob`] is the uploader's half of the protocol: it takes a blob off the
//!    list before the bytes are written, and waits out a deletion in progress.
//! 4. [`Store::claim_orphan_blob`] takes an object no row records (a crash can leave one) onto
//!    the same protocol, so the server's orphan sweep can delete it safely.
//!
//! Time is a parameter, so tests control it.

use std::time::Duration;

use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Result, Store};

/// What one expiry pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactExpiry {
    /// Available artifacts whose retention ran out.
    pub expired: u64,
    /// Upload requests nobody completed.
    pub abandoned: u64,
    /// Blobs that lost their last artifact and now wait out the grace period.
    pub unreferenced_blobs: u64,
    /// Old tombstones (expired artifact rows) deleted.
    pub tombstones_deleted: u64,
}

/// A stored blob nothing refers to any more: delete its object, then [`Store::forget_blob`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GarbageBlob {
    /// The project that owns it.
    pub project_id: Uuid,
    /// SHA-256, lowercase hex.
    pub sha256: String,
}

/// The answer to [`Store::reserve_blob`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobReservation {
    /// The blob is protected from collection: store the bytes.
    Reserved,
    /// The garbage collector is deleting it right now: retry shortly.
    BeingDeleted,
}

/// An upload nobody completed this long after its grant lapsed is abandoned. The server's spool
/// cleanup removes the bytes after two grant lifetimes, so no `complete` can still arrive.
const ABANDONED_AFTER: Duration = Duration::from_hours(24);
/// Rows one transaction expires.
const BATCH: i64 = 1000;

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

impl Store {
    /// Expires artifacts at `now`:
    ///
    /// 1. deletes tombstones (rows expired more than `tombstones` ago; `None` keeps them);
    /// 2. expires uploads abandoned for a day;
    /// 3. expires available artifacts past their `expires_at` unless on legal hold;
    /// 4. starts the grace period of every blob that lost its last reference.
    ///
    /// Idempotent. Reads nothing from the clock: `now` decides.
    pub async fn expire_artifacts(
        &self,
        now: OffsetDateTime,
        tombstones: Option<Duration>,
    ) -> Result<ArtifactExpiry> {
        let mut out = ArtifactExpiry::default();
        if let Some(keep) = tombstones {
            out.tombstones_deleted = sqlx::query!(
                "DELETE FROM artifacts
                  WHERE state = 'expired'
                    AND coalesce(expires_at, grant_expires_at, created_at)
                        < $1::timestamptz - make_interval(secs => $2)",
                now,
                secs(keep)
            )
            .execute(self.pool())
            .await?
            .rows_affected();
        }
        loop {
            let mut tx = self.pool().begin().await?;
            let lapsed = sqlx::query!(
                "UPDATE artifacts SET state = 'expired'
                  WHERE id IN (SELECT id FROM artifacts
                                WHERE state IN ('declared', 'uploading')
                                  AND grant_expires_at < $1::timestamptz - make_interval(secs => $2)
                                LIMIT $3)
              RETURNING project_id, encode(sha256, 'hex') AS \"sha256!\"",
                now,
                secs(ABANDONED_AFTER),
                BATCH
            )
            .fetch_all(&mut *tx)
            .await?;
            let due = sqlx::query!(
                "UPDATE artifacts SET state = 'expired'
                  WHERE id IN (SELECT id FROM artifacts
                                WHERE state = 'available' AND NOT legal_hold AND expires_at < $1
                                ORDER BY expires_at LIMIT $2)
              RETURNING project_id, encode(sha256, 'hex') AS \"sha256!\"",
                now,
                BATCH
            )
            .fetch_all(&mut *tx)
            .await?;
            out.abandoned += lapsed.len() as u64;
            out.expired += due.len() as u64;
            let (projects, hashes): (Vec<Uuid>, Vec<String>) = lapsed
                .into_iter()
                .map(|r| (r.project_id, r.sha256))
                .chain(due.iter().map(|r| (r.project_id, r.sha256.clone())))
                .unzip();
            // A separate statement: it must see the rows just expired as no longer referencing.
            // The grace period restarts for each: a blob re-referenced and released again
            // waits again.
            out.unreferenced_blobs += sqlx::query!(
                "UPDATE blobs b SET unreferenced_since = $1
                   FROM (SELECT DISTINCT project_id, sha256
                           FROM unnest($2::uuid[], $3::text[]) AS u(project_id, sha256)) k
                  WHERE b.project_id = k.project_id AND b.sha256 = decode(k.sha256, 'hex')
                    AND b.state = 'present'
                    AND NOT EXISTS (SELECT 1 FROM artifacts a
                                     WHERE a.project_id = b.project_id AND a.sha256 = b.sha256
                                       AND a.state IN ('declared', 'uploading', 'available'))",
                now,
                &projects,
                &hashes
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
            tx.commit().await?;
            if hashes.is_empty() || i64::try_from(hashes.len()).unwrap_or(i64::MAX) < BATCH {
                return Ok(out);
            }
        }
    }

    /// Claims up to `limit` blobs to delete at `now`: unreferenced for longer than `grace`, and
    /// still referenced by no artifact when checked again under the row lock (a request for the
    /// same bytes that arrived meanwhile saves the blob). Claimed blobs are marked `deleting`, and
    /// stay claimable until [`forget_blob`](Self::forget_blob): a crash between deleting the
    /// object and forgetting the row is retried (deleting a missing object is not an error).
    pub async fn claim_garbage_blobs(
        &self,
        now: OffsetDateTime,
        grace: Duration,
        limit: i64,
    ) -> Result<Vec<GarbageBlob>> {
        Ok(sqlx::query!(
            "UPDATE blobs b SET state = 'deleting'
              WHERE (b.project_id, b.sha256) IN (
                      SELECT c.project_id, c.sha256 FROM blobs c
                       WHERE c.state = 'deleting'
                          OR (c.state = 'present'
                              AND c.unreferenced_since < $1::timestamptz - make_interval(secs => $2)
                              AND NOT EXISTS (SELECT 1 FROM artifacts a
                                               WHERE a.project_id = c.project_id
                                                 AND a.sha256 = c.sha256
                                                 AND a.state IN ('declared', 'uploading',
                                                                 'available')))
                       ORDER BY c.unreferenced_since NULLS FIRST
                       LIMIT $3
                         FOR UPDATE SKIP LOCKED)
          RETURNING b.project_id, encode(b.sha256, 'hex') AS \"sha256!\"",
            now,
            secs(grace),
            limit
        )
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(|r| GarbageBlob {
            project_id: r.project_id,
            sha256: r.sha256,
        })
        .collect())
    }

    /// Forgets a blob whose object has been deleted.
    pub async fn forget_blob(&self, blob: &GarbageBlob) -> Result<()> {
        sqlx::query!(
            "DELETE FROM blobs
              WHERE project_id = $1 AND sha256 = decode($2, 'hex') AND state = 'deleting'",
            blob.project_id,
            blob.sha256
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Claims an object no `blobs` row records, for deletion. Its row goes in as `deleting`, the
    /// state the collector uses, so an upload of the same bytes waits for the delete (see
    /// [`reserve_blob`](Self::reserve_blob)). `false` when a row already exists: not an orphan.
    pub async fn claim_orphan_blob(&self, blob: &GarbageBlob, size: u64) -> Result<bool> {
        let claimed = sqlx::query!(
            "INSERT INTO blobs (project_id, sha256, size_bytes, state)
             VALUES ($1, decode($2, 'hex'), $3, 'deleting')
             ON CONFLICT (project_id, sha256) DO NOTHING",
            blob.project_id,
            blob.sha256,
            i64::try_from(size).unwrap_or(i64::MAX)
        )
        .execute(self.pool())
        .await?
        .rows_affected();
        Ok(claimed == 1)
    }

    /// Protects a blob that is about to be written: records it (or finds it) and takes it off
    /// the collector's list. Call it before writing the object; a blob the collector is deleting
    /// is refused until it is gone, so the write can never race the delete.
    pub async fn reserve_blob(
        &self,
        project_id: Uuid,
        sha256: &str,
        size: u64,
    ) -> Result<BlobReservation> {
        let kept = sqlx::query_scalar!(
            "INSERT INTO blobs (project_id, sha256, size_bytes)
             VALUES ($1, decode($2, 'hex'), $3)
             ON CONFLICT (project_id, sha256) DO UPDATE SET unreferenced_since = NULL
               WHERE blobs.state = 'present'
             RETURNING state",
            project_id,
            sha256,
            i64::try_from(size).unwrap_or(i64::MAX)
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(if kept.is_some() {
            BlobReservation::Reserved
        } else {
            BlobReservation::BeingDeleted
        })
    }
}

#[cfg(test)]
mod tests {
    use fleet_protocol::artifact::ArtifactClass;
    use sha2::{Digest, Sha256};
    use sqlx::PgPool;
    use time::Duration as Time;

    use super::*;
    use crate::{
        artifacts::{Quota, Requested, UploadAsk},
        auth::Publisher,
        ingest::{
            RunHandle, Transport,
            tests::{Agent, SuiteIds, publisher, suite_run},
        },
    };

    const DAY: Duration = Duration::from_hours(24);

    fn hex(content: &str) -> String {
        use std::fmt::Write as _;
        Sha256::digest(content.as_bytes())
            .iter()
            .fold(String::new(), |mut hex, b| {
                write!(hex, "{b:02x}").unwrap();
                hex
            })
    }

    /// A publisher, one run and its events: `events[..4]` leave the one flow running.
    struct Run {
        store: Store,
        publisher: Publisher,
        handle: RunHandle,
        events: Vec<fleet_protocol::journal::EventEnvelope>,
    }

    async fn run(pool: PgPool, ci_run: &str, passes: bool) -> Run {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let publisher = publisher(&store).await;
        run_in(store, publisher, ci_run, passes).await
    }

    async fn run_in(store: Store, publisher: Publisher, ci_run: &str, passes: bool) -> Run {
        let mut agent = Agent::new(ci_run, "e2e");
        let handle = store
            .open_run(&publisher, &agent.hello(None), Transport::WebSocket)
            .await
            .unwrap()
            .handle;
        let events = suite_run(&mut agent, SuiteIds::new(), &[("login", passes)]);
        Run {
            store,
            publisher,
            handle,
            events,
        }
    }

    impl Run {
        async fn apply(&self, range: std::ops::Range<usize>) {
            self.store
                .apply_batch(&self.handle, &self.events[range])
                .await
                .unwrap();
        }

        async fn flow(&self) -> Uuid {
            sqlx::query_scalar!(
                "SELECT id FROM flow_executions WHERE agent_run_id = $1",
                self.handle.agent_run_id
            )
            .fetch_one(self.store.pool())
            .await
            .unwrap()
        }

        /// Uploads `content` as `class` and returns the artifact: requested, received, verified.
        async fn upload(&self, class: ArtifactClass, flow: Option<Uuid>, content: &str) -> Uuid {
            let sha = hex(content);
            let ask = UploadAsk {
                project_id: self.publisher.project_id,
                credential: self.publisher.credential,
                agent_run_id: self.handle.agent_run_id,
                flow_execution_id: flow,
                step: None,
                class,
                path: "out/file",
                name: "file",
                content_type: "text/plain",
                size: content.len() as u64,
                sha256: &sha,
            };
            let quota = Quota {
                max_bytes: 1 << 30,
                max_files: 100,
                project_max_bytes: 1 << 40,
                project_max_files: 1_000_000,
            };
            let id = match self
                .store
                .request_upload(&ask, Duration::from_mins(15), quota)
                .await
                .unwrap()
            {
                Requested::Grant { artifact_id, .. } => artifact_id,
                Requested::Exists { artifact_id } => return artifact_id,
                other => panic!("unexpected {other:?}"),
            };
            let (project, credential) = (self.publisher.project_id, self.publisher.credential);
            self.store
                .claim_content(project, credential, id)
                .await
                .unwrap();
            assert_eq!(
                self.store
                    .reserve_blob(project, &sha, content.len() as u64)
                    .await
                    .unwrap(),
                BlobReservation::Reserved
            );
            self.store.content_received(id).await.unwrap();
            self.store
                .mark_available(project, id, "text/plain")
                .await
                .unwrap();
            id
        }

        async fn retention(&self, artifact: Uuid) -> (String, Time, String) {
            let r = sqlx::query!(
                "SELECT retention_class, expires_at AS \"expires_at!\", available_at AS \"available_at!\",
                        state
                   FROM artifacts WHERE id = $1",
                artifact
            )
            .fetch_one(self.store.pool())
            .await
            .unwrap();
            (r.retention_class, r.expires_at - r.available_at, r.state)
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn failure_evidence_is_promoted_when_its_flow_finalizes_as_failed(pool: PgPool) {
        let run = run(pool, "1", false).await;
        run.apply(0..4).await;
        let flow = run.flow().await;
        let shot = run
            .upload(ArtifactClass::FailureScreenshot, Some(flow), "shot")
            .await;
        let log = run.upload(ArtifactClass::MaestroLog, None, "log").await;
        let report = run.upload(ArtifactClass::Junit, None, "junit").await;
        // While the flow runs nothing is known: the defaults of research 05 §12.3.
        assert_eq!(
            run.retention(shot).await,
            ("success_evidence".into(), Time::days(7), "available".into())
        );
        assert_eq!(
            run.retention(log).await,
            ("debug_log".into(), Time::days(30), "available".into())
        );

        run.apply(4..5).await;

        assert_eq!(
            run.retention(shot).await,
            (
                "failure_evidence".into(),
                Time::days(90),
                "available".into()
            )
        );
        assert_eq!(
            run.retention(log).await,
            (
                "failure_evidence".into(),
                Time::days(90),
                "available".into()
            )
        );
        // A report keeps its own class.
        assert_eq!(
            run.retention(report).await,
            ("report".into(), Time::days(365), "available".into())
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn evidence_of_a_passing_flow_keeps_the_short_retention(pool: PgPool) {
        let run = run(pool, "2", true).await;
        run.apply(0..4).await;
        let flow = run.flow().await;
        let shot = run
            .upload(ArtifactClass::FailureScreenshot, Some(flow), "shot")
            .await;
        let log = run.upload(ArtifactClass::MaestroLog, None, "log").await;

        run.apply(4..run.events.len()).await;

        assert_eq!(
            run.retention(shot).await,
            ("success_evidence".into(), Time::days(7), "available".into())
        );
        assert_eq!(
            run.retention(log).await,
            ("debug_log".into(), Time::days(30), "available".into())
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn artifacts_expire_with_their_class_and_a_legal_hold_keeps_them(pool: PgPool) {
        let run = run(pool, "3", true).await;
        run.apply(0..4).await;
        let flow = run.flow().await;
        let short = run
            .upload(ArtifactClass::FailureScreenshot, Some(flow), "short")
            .await;
        let held = run
            .upload(ArtifactClass::Commands, Some(flow), "held")
            .await;
        let report = run.upload(ArtifactClass::Junit, None, "report").await;
        sqlx::query!("UPDATE artifacts SET legal_hold = true WHERE id = $1", held)
            .execute(run.store.pool())
            .await
            .unwrap();

        // Seven days and a bit: only the 7-day evidence is due.
        let now = OffsetDateTime::now_utc() + Time::days(8);
        let done = run.store.expire_artifacts(now, None).await.unwrap();

        assert_eq!(done.expired, 1);
        assert_eq!(run.retention(short).await.2, "expired");
        assert_eq!(run.retention(held).await.2, "available");
        assert_eq!(run.retention(report).await.2, "available");
        // Run it again: nothing more is due.
        assert_eq!(
            run.store.expire_artifacts(now, None).await.unwrap().expired,
            0
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_shared_blob_is_collected_only_after_its_last_artifact_and_the_grace_period(
        pool: PgPool,
    ) {
        let first = run(pool, "4", true).await;
        let second = run_in(first.store.clone(), first.publisher.clone(), "5", true).await;
        first.apply(0..4).await;
        second.apply(0..4).await;
        // The same bytes, uploaded by two runs of one project, are one blob.
        let a = first
            .upload(ArtifactClass::MaestroLog, None, "same bytes")
            .await;
        let b = second
            .upload(ArtifactClass::MaestroLog, None, "same bytes")
            .await;
        assert_ne!(a, b);
        let blobs = sqlx::query_scalar!("SELECT count(*) FROM blobs")
            .fetch_one(first.store.pool())
            .await
            .unwrap();
        assert_eq!(blobs, Some(1));
        // The second copy lives longer (as if promoted).
        let later = OffsetDateTime::now_utc() + Time::days(60);
        sqlx::query!(
            "UPDATE artifacts SET expires_at = $2 WHERE id = $1",
            b,
            later
        )
        .execute(first.store.pool())
        .await
        .unwrap();
        let grace = DAY;

        // The first copy expires: the bytes are still the second copy's.
        let t1 = OffsetDateTime::now_utc() + Time::days(31);
        let done = first.store.expire_artifacts(t1, None).await.unwrap();
        assert_eq!((done.expired, done.unreferenced_blobs), (1, 0));
        let far = t1 + Time::days(30);
        assert_eq!(
            first
                .store
                .claim_garbage_blobs(far, grace, 10)
                .await
                .unwrap(),
            []
        );

        // The second copy expires: the blob starts its grace period.
        let t2 = later + Time::seconds(1);
        let done = first.store.expire_artifacts(t2, None).await.unwrap();
        assert_eq!((done.expired, done.unreferenced_blobs), (1, 1));
        assert_eq!(
            first
                .store
                .claim_garbage_blobs(t2 + Time::hours(23), grace, 10)
                .await
                .unwrap(),
            [],
            "still within the grace period"
        );
        let due = first
            .store
            .claim_garbage_blobs(t2 + Time::hours(25), grace, 10)
            .await
            .unwrap();
        assert_eq!(
            due,
            [GarbageBlob {
                project_id: first.publisher.project_id,
                sha256: hex("same bytes")
            }]
        );

        // Until the object is deleted and forgotten, a crash would retry it.
        let again = first
            .store
            .claim_garbage_blobs(t2 + Time::hours(26), grace, 10)
            .await
            .unwrap();
        assert_eq!(again, due);
        first.store.forget_blob(&due[0]).await.unwrap();
        assert_eq!(
            first
                .store
                .claim_garbage_blobs(t2 + Time::hours(27), grace, 10)
                .await
                .unwrap(),
            []
        );
        let blobs = sqlx::query_scalar!("SELECT count(*) FROM blobs")
            .fetch_one(first.store.pool())
            .await
            .unwrap();
        assert_eq!(blobs, Some(0));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_new_upload_of_the_same_bytes_during_the_grace_period_saves_the_blob(pool: PgPool) {
        let first = run(pool, "6", true).await;
        first.apply(0..4).await;
        first
            .upload(ArtifactClass::MaestroLog, None, "precious")
            .await;
        let t = OffsetDateTime::now_utc() + Time::days(31);
        first.store.expire_artifacts(t, None).await.unwrap();

        // Another run asks to upload the same bytes before the grace period ends. Its request
        // alone (declared, not yet stored) keeps the collector away.
        let second = run_in(first.store.clone(), first.publisher.clone(), "7", true).await;
        second.apply(0..4).await;
        let sha = hex("precious");
        let ask = UploadAsk {
            project_id: second.publisher.project_id,
            credential: second.publisher.credential,
            agent_run_id: second.handle.agent_run_id,
            flow_execution_id: None,
            step: None,
            class: ArtifactClass::MaestroLog,
            path: "out/file",
            name: "file",
            content_type: "text/plain",
            size: 8,
            sha256: &sha,
        };
        let quota = Quota {
            max_bytes: 1 << 30,
            max_files: 100,
            project_max_bytes: 1 << 40,
            project_max_files: 1_000_000,
        };
        let requested = second
            .store
            .request_upload(&ask, Duration::from_mins(15), quota)
            .await
            .unwrap();
        assert!(
            matches!(requested, Requested::Grant { .. }),
            "{requested:?}"
        );

        let due = first
            .store
            .claim_garbage_blobs(t + Time::days(3), DAY, 10)
            .await
            .unwrap();

        assert_eq!(due, [], "the pending upload references the blob");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_blob_being_deleted_cannot_be_reserved_until_it_is_forgotten(pool: PgPool) {
        let run = run(pool, "8", true).await;
        run.apply(0..4).await;
        run.upload(ArtifactClass::MaestroLog, None, "doomed").await;
        let t = OffsetDateTime::now_utc() + Time::days(31);
        run.store.expire_artifacts(t, None).await.unwrap();
        let due = run
            .store
            .claim_garbage_blobs(t + Time::days(2), DAY, 10)
            .await
            .unwrap();
        let project = run.publisher.project_id;

        assert_eq!(
            run.store
                .reserve_blob(project, &hex("doomed"), 6)
                .await
                .unwrap(),
            BlobReservation::BeingDeleted
        );
        run.store.forget_blob(&due[0]).await.unwrap();
        assert_eq!(
            run.store
                .reserve_blob(project, &hex("doomed"), 6)
                .await
                .unwrap(),
            BlobReservation::Reserved
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn abandoned_requests_expire_and_old_tombstones_are_deleted(pool: PgPool) {
        let run = run(pool, "9", true).await;
        run.apply(0..4).await;
        let sha = hex("never sent");
        let ask = UploadAsk {
            project_id: run.publisher.project_id,
            credential: run.publisher.credential,
            agent_run_id: run.handle.agent_run_id,
            flow_execution_id: None,
            step: None,
            class: ArtifactClass::MaestroLog,
            path: "out/file",
            name: "file",
            content_type: "text/plain",
            size: 10,
            sha256: &sha,
        };
        let quota = Quota {
            max_bytes: 1 << 30,
            max_files: 100,
            project_max_bytes: 1 << 40,
            project_max_files: 1_000_000,
        };
        run.store
            .request_upload(&ask, Duration::from_mins(15), quota)
            .await
            .unwrap();
        let sent = run.upload(ArtifactClass::Commands, None, "sent").await;
        let now = OffsetDateTime::now_utc();

        // A grant that lapsed minutes ago is not abandoned yet; one that lapsed a day ago is.
        let early = run
            .store
            .expire_artifacts(now + Time::hours(1), None)
            .await
            .unwrap();
        assert_eq!(early.abandoned, 0);
        let later = run
            .store
            .expire_artifacts(now + Time::days(2), None)
            .await
            .unwrap();
        assert_eq!(later.abandoned, 1);
        let states = sqlx::query_scalar!("SELECT state FROM artifacts ORDER BY created_at")
            .fetch_all(run.store.pool())
            .await
            .unwrap();
        assert_eq!(states, ["expired", "available"]);

        // Tombstones (expired rows) go once they are older than the configured time, and only
        // in a later pass than the one that expired them. Live artifacts never go.
        let kept = run
            .store
            .expire_artifacts(now + Time::days(3), Some(DAY * 30))
            .await
            .unwrap();
        assert_eq!(kept.tombstones_deleted, 0);
        let day40 = run
            .store
            .expire_artifacts(now + Time::days(40), Some(DAY * 30))
            .await
            .unwrap();
        assert_eq!(
            (day40.tombstones_deleted, day40.expired),
            (1, 1),
            "the abandoned row; sent expires"
        );
        assert_eq!(
            sqlx::query_scalar!("SELECT id FROM artifacts")
                .fetch_all(run.store.pool())
                .await
                .unwrap(),
            [sent]
        );
        let day80 = run
            .store
            .expire_artifacts(now + Time::days(80), Some(DAY * 30))
            .await
            .unwrap();
        assert_eq!(day80.tombstones_deleted, 1);
        assert_eq!(
            sqlx::query_scalar!("SELECT count(*) FROM artifacts")
                .fetch_one(run.store.pool())
                .await
                .unwrap(),
            Some(0)
        );
    }
}
