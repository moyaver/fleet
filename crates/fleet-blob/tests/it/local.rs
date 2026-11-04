// SPDX-License-Identifier: AGPL-3.0-only
//! The contract against the local filesystem backend.

use bytes::Bytes;
use fleet_blob::{BlobStore, ByteStream, LocalFs, Scope};
use futures_util::{StreamExt, stream};

use crate::contract;

/// A directory under the system temp dir, removed on drop.
pub(crate) struct TempDir(pub(crate) std::path::PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("fleet-blob-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn local_fs_meets_the_contract() {
    let dir = TempDir::new();
    let store = LocalFs::new(dir.0.join("blobs")).unwrap();
    contract::run(&store).await;
}

/// Regular files under `dir`, recursively.
fn files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn failed_and_cancelled_puts_leave_no_files_behind() {
    let dir = TempDir::new();
    let store = LocalFs::new(dir.0.join("blobs")).unwrap();
    let key = contract::key_of(contract::project(), b"expected");

    store
        .put(&key, 8, contract::body(b"mismatch"))
        .await
        .unwrap_err();
    let broken: ByteStream<'static> = stream::iter([
        Ok(Bytes::from_static(b"exp")),
        Err(std::io::Error::other("reset")),
    ])
    .boxed();
    store.put(&key, 8, broken).await.unwrap_err();

    // A body that never ends, dropped half-way (client disconnect, shutdown).
    let stalled: ByteStream<'static> = stream::once(async { Ok(Bytes::from_static(b"exp")) })
        .chain(stream::pending())
        .boxed();
    let put = store.put(&key, 8, stalled);
    tokio::time::timeout(std::time::Duration::from_millis(100), put)
        .await
        .unwrap_err();

    assert_eq!(
        files(&dir.0.join("blobs")),
        Vec::<std::path::PathBuf>::new()
    );
}

#[tokio::test]
async fn listing_skips_files_that_are_not_blobs() {
    let dir = TempDir::new();
    let root = dir.0.join("blobs");
    let store = LocalFs::new(&root).unwrap();
    let project = contract::project();
    let key = contract::key_of(project, b"real");
    store.put(&key, 4, contract::body(b"real")).await.unwrap();
    // Debris an operator or a crashed upload might leave.
    std::fs::create_dir_all(root.join(".tmp")).unwrap();
    std::fs::write(root.join(".tmp/half-written"), b"x").unwrap();
    std::fs::create_dir_all(root.join(project.to_string()).join("zz")).unwrap();
    std::fs::write(root.join(project.to_string()).join("zz/junk"), b"x").unwrap();
    std::fs::write(root.join("README"), b"x").unwrap();

    let listed: Vec<_> = store
        .list(Scope::All)
        .map(|i| i.unwrap().key)
        .collect()
        .await;
    assert_eq!(listed, vec![key]);
}
