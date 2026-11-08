// SPDX-License-Identifier: AGPL-3.0-only
//! The contract against Garage, the reference S3 store (PLAN D5, ADR-0007), and direct-upload
//! staging against it: a presigned `PUT` must carry the signed checksum, and what it stores is
//! what the server reads back.

use std::{collections::BTreeMap, time::Duration};

use base64::Engine as _;
use fleet_blob::{BlobStore, Sha256Digest, StagingKey, testing::Garage};
use futures_util::TryStreamExt;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::contract;

fn checksum_header(bytes: &[u8]) -> (String, String) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(Sha256Digest::of(bytes).as_bytes());
    ("x-amz-checksum-sha256".to_owned(), b64)
}

#[tokio::test]
async fn garage_meets_the_contract() {
    let garage = Garage::start().unwrap_or_else(|e| panic!("{e}"));
    let store = garage.ready_store().await.unwrap();
    contract::run(&store).await;
}

/// Presigns a `PUT` of `bytes` to a fresh staging key, bound to their checksum.
async fn presigned(
    store: &impl BlobStore,
    bytes: &[u8],
) -> (StagingKey, String, BTreeMap<String, String>) {
    let staging = StagingKey::new(Uuid::now_v7());
    let (name, value) = checksum_header(bytes);
    let headers = BTreeMap::from([(name, value)]);
    let url = store
        .presign_put(&staging, Duration::from_secs(300), &headers)
        .await
        .unwrap()
        .expect("S3 can presign");
    (staging, url, headers)
}

#[tokio::test]
async fn a_presigned_put_stages_the_bytes_it_was_signed_for() {
    let garage = Garage::start().unwrap_or_else(|e| panic!("{e}"));
    let store = garage.ready_store().await.unwrap();
    let bytes = b"hello direct upload".to_vec();
    let (staging, url, headers) = presigned(&store, &bytes).await;

    let mut put = reqwest::Client::new().put(&url).body(bytes.clone());
    for (name, value) in &headers {
        put = put.header(name, value);
    }
    let sent = put.send().await.unwrap();
    assert!(sent.status().is_success(), "{}", sent.status());

    let staged = store.staged(&staging).await.unwrap().expect("staged");
    assert_eq!(staged.size, bytes.len() as u64);
    let read = staged
        .body
        .try_fold(Vec::new(), |mut all, chunk| async move {
            all.extend_from_slice(&chunk);
            Ok(all)
        })
        .await
        .unwrap();
    assert_eq!(read, bytes);

    store.delete_staged(&staging).await.unwrap();
    assert!(store.staged(&staging).await.unwrap().is_none());
    // Deleting what is gone is not an error.
    store.delete_staged(&staging).await.unwrap();
}

#[tokio::test]
async fn bytes_that_do_not_match_the_signed_checksum_are_never_staged() {
    let garage = Garage::start().unwrap_or_else(|e| panic!("{e}"));
    let store = garage.ready_store().await.unwrap();
    let (staging, url, headers) = presigned(&store, b"the declared file").await;
    let client = reqwest::Client::new();

    // The right header, other bytes: the object store refuses them.
    let (name, value) = headers.iter().next().unwrap();
    let wrong = client
        .put(&url)
        .header(name, value)
        .body(b"other bytes".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 400, "{}", wrong.text().await.unwrap());

    // Without the signed header the request is not the one that was signed.
    let unsigned = client
        .put(&url)
        .body(b"the declared file".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status(), 400);

    assert!(store.staged(&staging).await.unwrap().is_none());
}

#[tokio::test]
async fn a_sweep_removes_only_stale_unkept_staged_objects() {
    let garage = Garage::start().unwrap_or_else(|e| panic!("{e}"));
    let store = garage.ready_store().await.unwrap();
    let (kept, kept_url, kept_headers) = presigned(&store, b"kept").await;
    let (gone, gone_url, gone_headers) = presigned(&store, b"abandoned").await;
    let client = reqwest::Client::new();
    for (url, headers, body) in [
        (&kept_url, &kept_headers, &b"kept"[..]),
        (&gone_url, &gone_headers, &b"abandoned"[..]),
    ] {
        let (name, value) = headers.iter().next().unwrap();
        let sent = client
            .put(url)
            .header(name, value)
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        assert!(sent.status().is_success());
    }

    // "Older than an hour from now" makes both stale; only the unkept one goes.
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    let removed = store.sweep_staged(&[kept.artifact()], later).await.unwrap();
    assert_eq!(removed, 1);
    assert!(store.staged(&kept).await.unwrap().is_some());
    assert!(store.staged(&gone).await.unwrap().is_none());
}
