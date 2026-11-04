// SPDX-License-Identifier: AGPL-3.0-only
//! Local filesystem backend.
//!
//! Layout: `<root>/<project>/<shard>/<sha256>`, plus `<root>/.tmp/` for uploads in progress. A
//! blob appears at its final path only by `rename` of a fully written, `fsync`ed temp file, so a
//! crash leaves either no blob or a complete one. Paths are built from a [`BlobKey`] (a UUID and
//! 64 hex digits), never from caller text, so nothing can name a file outside the root.

use std::{
    io::{self, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
};

use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{
    BlobError, BlobInfo, BlobKey, BlobRead, BlobStore, ByteStream, PutOutcome, Scope, check::Check,
    store::resolve_range,
};

/// Directory (under the root) holding uploads in progress.
const TMP_DIR: &str = ".tmp";

/// Blobs in a directory.
#[derive(Debug, Clone)]
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    /// A store rooted at `root`, which is created if missing.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, BlobError> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|e| BlobError::backend("creating the blob directory", e))?;
        Ok(Self { root })
    }

    fn path(&self, key: &BlobKey) -> PathBuf {
        self.root.join(key.path())
    }
}

/// A temp file that is removed unless [`persist`](Self::persist) was called: a failed or
/// cancelled `put` leaves nothing behind.
struct TempFile {
    path: PathBuf,
    keep: bool,
}

impl TempFile {
    fn persist(mut self) {
        self.keep = true;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn io_err(context: &'static str) -> impl FnOnce(io::Error) -> BlobError {
    move |e| BlobError::backend(context, e)
}

fn not_found_as_none<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Makes a directory entry durable: without this a rename can vanish in a crash.
async fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        tokio::fs::File::open(dir).await?.sync_all().await
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[async_trait::async_trait]
impl BlobStore for LocalFs {
    async fn put(
        &self,
        key: &BlobKey,
        size: u64,
        mut body: ByteStream<'_>,
    ) -> Result<PutOutcome, BlobError> {
        let path = self.path(key);
        let mut check = Check::new(key, size);
        let exists = tokio::fs::try_exists(&path)
            .await
            .map_err(io_err("checking for the blob"))?;
        if exists {
            // Still prove the caller has these bytes; just don't write them again.
            while let Some(chunk) = body.next().await {
                check.feed(&chunk.map_err(BlobError::Body)?)?;
            }
            check.finish()?;
            return Ok(PutOutcome::AlreadyExists);
        }

        let tmp_dir = self.root.join(TMP_DIR);
        tokio::fs::create_dir_all(&tmp_dir)
            .await
            .map_err(io_err("creating the temp directory"))?;
        let tmp = TempFile {
            path: tmp_dir.join(Uuid::now_v7().simple().to_string()),
            keep: false,
        };
        let mut file = tokio::fs::File::create(&tmp.path)
            .await
            .map_err(io_err("creating the temp file"))?;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(BlobError::Body)?;
            check.feed(&chunk)?;
            file.write_all(&chunk)
                .await
                .map_err(io_err("writing the blob"))?;
        }
        check.finish()?;
        file.sync_all().await.map_err(io_err("syncing the blob"))?;
        drop(file);

        let shard_dir = path
            .parent()
            .ok_or_else(|| BlobError::backend("locating the blob directory", "no parent"))?;
        let shard_is_new = !tokio::fs::try_exists(shard_dir).await.unwrap_or(false);
        tokio::fs::create_dir_all(shard_dir)
            .await
            .map_err(io_err("creating the blob directory"))?;
        tokio::fs::rename(&tmp.path, &path)
            .await
            .map_err(io_err("publishing the blob"))?;
        tmp.persist();
        sync_dir(shard_dir)
            .await
            .map_err(io_err("syncing the blob directory"))?;
        if shard_is_new {
            // The new directories' own entries.
            for dir in shard_dir.ancestors().skip(1).take(2) {
                sync_dir(dir)
                    .await
                    .map_err(io_err("syncing the blob directory"))?;
            }
        }
        Ok(PutOutcome::Created)
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<BlobRead, BlobError> {
        let mut file = not_found_as_none(tokio::fs::File::open(self.path(key)).await)
            .map_err(io_err("opening the blob"))?
            .ok_or(BlobError::NotFound)?;
        let size = file
            .metadata()
            .await
            .map_err(io_err("reading the blob size"))?
            .len();
        let range = resolve_range(size, range)?;
        file.seek(SeekFrom::Start(range.start))
            .await
            .map_err(io_err("seeking in the blob"))?;
        let body = ReaderStream::new(file.take(range.end - range.start)).boxed();
        Ok(BlobRead { size, range, body })
    }

    async fn head(&self, key: &BlobKey) -> Result<Option<BlobInfo>, BlobError> {
        let meta = not_found_as_none(tokio::fs::metadata(self.path(key)).await)
            .map_err(io_err("reading the blob"))?;
        meta.map(|m| info(*key, &m)).transpose()
    }

    async fn delete(&self, key: &BlobKey) -> Result<(), BlobError> {
        not_found_as_none(tokio::fs::remove_file(self.path(key)).await)
            .map(|_| ())
            .map_err(io_err("deleting the blob"))
    }

    async fn sweep_stale_uploads(&self, older_than: OffsetDateTime) -> Result<u64, BlobError> {
        let tmp = self.root.join(TMP_DIR);
        let cutoff = std::time::SystemTime::from(older_than);
        tokio::task::spawn_blocking(move || {
            let mut removed = 0;
            for file in read_dir(&tmp)? {
                // Gone, or finished (renamed away), since the directory was read: not an error.
                let Some(meta) = not_found_as_none(std::fs::metadata(&file))
                    .map_err(io_err("reading a temp file"))?
                else {
                    continue;
                };
                let stale = meta.is_file()
                    && meta
                        .modified()
                        .map_err(io_err("reading a temp file's modification time"))?
                        < cutoff;
                if stale
                    && not_found_as_none(std::fs::remove_file(&file))
                        .map_err(io_err("removing a stale temp file"))?
                        .is_some()
                {
                    tracing::info!(path = %file.display(), "removed a stale upload temp file");
                    removed += 1;
                }
            }
            Ok(removed)
        })
        .await
        .map_err(|e| BlobError::backend("sweeping temp files", e))?
    }

    fn list(&self, scope: Scope) -> BoxStream<'_, Result<BlobInfo, BlobError>> {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = walk(&root, scope, &tx) {
                let _ = tx.blocking_send(Err(e));
            }
        });
        stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed()
    }
}

fn info(key: BlobKey, meta: &std::fs::Metadata) -> Result<BlobInfo, BlobError> {
    let modified = meta
        .modified()
        .map_err(io_err("reading the blob's modification time"))?;
    Ok(BlobInfo {
        key,
        size: meta.len(),
        last_modified: OffsetDateTime::from(modified),
    })
}

type Sender = tokio::sync::mpsc::Sender<Result<BlobInfo, BlobError>>;

/// Visits `<root>/<project>/<shard>/<file>`; a file that does not parse as a key is not ours.
/// Stops quietly when the receiver is gone.
fn walk(root: &Path, scope: Scope, tx: &Sender) -> Result<(), BlobError> {
    let projects: Vec<PathBuf> = match scope {
        Scope::Project(id) => vec![root.join(id.hyphenated().to_string())],
        // `.tmp` and anything else that is not a project directory is not ours.
        Scope::All => read_dir(root)?
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| Uuid::try_parse(n).is_ok())
            })
            .collect(),
    };
    for project in projects {
        for shard in read_dir(&project)? {
            for file in read_dir(&shard)? {
                let Some(key) = file
                    .strip_prefix(root)
                    .ok()
                    .and_then(|p| p.to_str())
                    .map(|p| p.replace(std::path::MAIN_SEPARATOR, "/"))
                    .and_then(|p| BlobKey::from_path(&p))
                else {
                    tracing::debug!(path = %file.display(), "skipping a file that is not a blob");
                    continue;
                };
                // Deleted since the directory was read: not an error.
                let Some(meta) = not_found_as_none(std::fs::metadata(&file))
                    .map_err(io_err("reading the blob"))?
                else {
                    continue;
                };
                if tx.blocking_send(info(key, &meta)).is_err() {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Child paths; a missing directory, or a file where a directory should be, has none.
fn read_dir(dir: &Path) -> Result<Vec<PathBuf>, BlobError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Vec::new());
        }
        Err(e) => return Err(BlobError::backend("listing the blob directory", e)),
    };
    entries
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<_>>()
        .map_err(io_err("listing the blob directory"))
}
