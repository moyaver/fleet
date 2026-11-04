// SPDX-License-Identifier: AGPL-3.0-only
//! The `BlobStore` contract (S8). Every backend runs [`run`]; a backend that needs a special
//! case is not interchangeable (ADR-0007).

use bytes::Bytes;
use fleet_blob::{BlobError, BlobKey, BlobStore, ByteStream, PutOutcome, Scope, Sha256Digest};
use futures_util::{StreamExt, stream};
use time::OffsetDateTime;
use uuid::Uuid;

/// A body of one chunk.
pub(crate) fn body(bytes: &[u8]) -> ByteStream<'static> {
    stream::iter([Ok(Bytes::copy_from_slice(bytes))]).boxed()
}

/// A fresh project id, so cases never see each other's blobs in a shared bucket.
pub(crate) fn project() -> Uuid {
    Uuid::now_v7()
}

pub(crate) fn key_of(project: Uuid, bytes: &[u8]) -> BlobKey {
    BlobKey::new(project, Sha256Digest::of(bytes))
}

pub(crate) async fn read_all(store: &(impl BlobStore + ?Sized), key: &BlobKey) -> Vec<u8> {
    let read = store.get(key, None).await.expect("get should succeed");
    let mut out = Vec::new();
    let mut body = read.body;
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk.expect("body chunk"));
    }
    out
}

/// Runs every case against `store`.
pub(crate) async fn run<S: BlobStore + ?Sized>(store: &S) {
    put_then_get_round_trips(store).await;
    an_empty_blob_is_a_blob(store).await;
    putting_twice_is_idempotent(store).await;
    a_wrong_digest_is_refused_and_stores_nothing(store).await;
    a_wrong_size_is_refused_and_stores_nothing(store).await;
    a_failed_body_stores_nothing(store).await;
    a_bad_put_never_damages_an_existing_blob(store).await;
    missing_blobs_and_idempotent_delete(store).await;
    ranges_follow_http_semantics(store).await;
    projects_are_isolated(store).await;
    listing_finds_a_projects_blobs_for_gc(store).await;
    a_large_blob_streams_through(store).await;
}

async fn put_then_get_round_trips<S: BlobStore + ?Sized>(store: &S) {
    let (project, data) = (project(), b"hello fleet".as_slice());
    let key = key_of(project, data);
    let outcome = store.put(&key, data.len() as u64, body(data)).await;
    assert!(matches!(outcome, Ok(PutOutcome::Created)), "{outcome:?}");
    assert_eq!(read_all(store, &key).await, data);
    let info = store.head(&key).await.unwrap().expect("head after put");
    assert_eq!((info.key, info.size), (key, 11));
}

async fn an_empty_blob_is_a_blob<S: BlobStore + ?Sized>(store: &S) {
    let key = key_of(project(), b"");
    store.put(&key, 0, body(b"")).await.unwrap();
    assert_eq!(read_all(store, &key).await, b"");
    assert_eq!(store.head(&key).await.unwrap().unwrap().size, 0);
}

async fn putting_twice_is_idempotent<S: BlobStore + ?Sized>(store: &S) {
    let (project, data) = (project(), b"same bytes".as_slice());
    let key = key_of(project, data);
    let first = store.put(&key, 10, body(data)).await.unwrap();
    let second = store.put(&key, 10, body(data)).await.unwrap();
    assert_eq!(
        (first, second),
        (PutOutcome::Created, PutOutcome::AlreadyExists)
    );
    assert_eq!(read_all(store, &key).await, data);
}

async fn a_wrong_digest_is_refused_and_stores_nothing<S: BlobStore + ?Sized>(store: &S) {
    let key = key_of(project(), b"what the key says");
    let err = store
        .put(&key, 17, body(b"what was sent!!!!"))
        .await
        .unwrap_err();
    assert!(matches!(err, BlobError::DigestMismatch { .. }), "{err:?}");
    assert!(store.head(&key).await.unwrap().is_none());
}

async fn a_wrong_size_is_refused_and_stores_nothing<S: BlobStore + ?Sized>(store: &S) {
    let (project, data) = (project(), b"0123456789".as_slice());
    let key = key_of(project, data);
    let too_short = store.put(&key, 11, body(data)).await.unwrap_err();
    assert!(
        matches!(
            too_short,
            BlobError::SizeMismatch {
                declared: 11,
                received: 10
            }
        ),
        "{too_short:?}"
    );
    // Longer than declared: refused without reading on to the end.
    let too_long = store.put(&key, 4, body(data)).await.unwrap_err();
    assert!(
        matches!(too_long, BlobError::SizeMismatch { declared: 4, .. }),
        "{too_long:?}"
    );
    assert!(store.head(&key).await.unwrap().is_none());
}

async fn a_failed_body_stores_nothing<S: BlobStore + ?Sized>(store: &S) {
    let (project, data) = (project(), b"first half|second half".as_slice());
    let key = key_of(project, data);
    let broken: ByteStream<'static> = stream::iter([
        Ok(Bytes::from_static(b"first half|")),
        Err(std::io::Error::other("client went away")),
    ])
    .boxed();
    let err = store
        .put(&key, data.len() as u64, broken)
        .await
        .unwrap_err();
    assert!(matches!(err, BlobError::Body(_)), "{err:?}");
    assert!(store.head(&key).await.unwrap().is_none());
}

async fn a_bad_put_never_damages_an_existing_blob<S: BlobStore + ?Sized>(store: &S) {
    let (project, data) = (project(), b"precious evidence".as_slice());
    let key = key_of(project, data);
    store.put(&key, 17, body(data)).await.unwrap();
    let err = store
        .put(&key, 17, body(b"forged evidence!!"))
        .await
        .unwrap_err();
    assert!(matches!(err, BlobError::DigestMismatch { .. }), "{err:?}");
    assert_eq!(read_all(store, &key).await, data);
}

async fn missing_blobs_and_idempotent_delete<S: BlobStore + ?Sized>(store: &S) {
    let (project, data) = (project(), b"to be deleted".as_slice());
    let key = key_of(project, data);
    assert!(matches!(
        store.get(&key, None).await,
        Err(BlobError::NotFound)
    ));
    assert!(store.head(&key).await.unwrap().is_none());
    store
        .delete(&key)
        .await
        .expect("deleting a missing blob is fine");
    store.put(&key, 13, body(data)).await.unwrap();
    store.delete(&key).await.unwrap();
    assert!(matches!(
        store.get(&key, None).await,
        Err(BlobError::NotFound)
    ));
    store.delete(&key).await.expect("and so is deleting twice");
}

async fn ranges_follow_http_semantics<S: BlobStore + ?Sized>(store: &S) {
    let (project, data) = (project(), b"0123456789".as_slice());
    let key = key_of(project, data);
    store.put(&key, 10, body(data)).await.unwrap();

    let read_range = |range: std::ops::Range<u64>| async move {
        let read = store.get(&key, Some(range)).await?;
        let mut out = Vec::new();
        let mut stream = read.body;
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        Ok::<_, BlobError>((read.size, read.range, out))
    };
    assert_eq!(read_range(2..5).await.unwrap(), (10, 2..5, b"234".to_vec()));
    assert_eq!(
        read_range(8..100).await.unwrap(),
        (10, 8..10, b"89".to_vec())
    );
    assert_eq!(read_range(0..1).await.unwrap(), (10, 0..1, b"0".to_vec()));
    let reversed = std::ops::Range { start: 7, end: 3 };
    for unsatisfiable in [10..11, 99..100, 5..5, reversed] {
        let err = read_range(unsatisfiable.clone()).await.unwrap_err();
        assert!(
            matches!(err, BlobError::RangeNotSatisfiable { size: 10 }),
            "{unsatisfiable:?}: {err:?}"
        );
    }
}

async fn projects_are_isolated<S: BlobStore + ?Sized>(store: &S) {
    let (a, b, data) = (project(), project(), b"shared screenshot".as_slice());
    let (key_a, key_b) = (key_of(a, data), key_of(b, data));
    assert_eq!(
        store.put(&key_a, 17, body(data)).await.unwrap(),
        PutOutcome::Created
    );
    assert!(
        store.head(&key_b).await.unwrap().is_none(),
        "b cannot see a's blob"
    );
    assert_eq!(
        store.put(&key_b, 17, body(data)).await.unwrap(),
        PutOutcome::Created
    );
    store.delete(&key_a).await.unwrap();
    assert_eq!(
        read_all(store, &key_b).await,
        data,
        "deleting in a leaves b"
    );
}

async fn listing_finds_a_projects_blobs_for_gc<S: BlobStore + ?Sized>(store: &S) {
    let (a, b) = (project(), project());
    let blobs = [
        (a, "one"),
        (a, "second"),
        (a, "the third"),
        (b, "other project"),
    ];
    for (p, text) in blobs {
        let data = text.as_bytes();
        store
            .put(&key_of(p, data), data.len() as u64, body(data))
            .await
            .unwrap();
    }
    let collect = |scope| async move {
        let mut found: Vec<_> = store
            .list(scope)
            .map(|item| item.expect("list item"))
            .collect()
            .await;
        found.sort_by_key(|i| i.size);
        found
    };

    let in_a = collect(Scope::Project(a)).await;
    let seen: Vec<_> = in_a.iter().map(|i| (i.key, i.size)).collect();
    let expected: Vec<_> = [(a, "one"), (a, "second"), (a, "the third")]
        .iter()
        .map(|(p, t)| (key_of(*p, t.as_bytes()), t.len() as u64))
        .collect();
    assert_eq!(seen, expected, "exactly project a's blobs, with sizes");
    let age = (OffsetDateTime::now_utc() - in_a[0].last_modified).abs();
    assert!(
        age < time::Duration::minutes(5),
        "last_modified is the write time: {age}"
    );

    let everything = collect(Scope::All).await;
    let b_key = key_of(b, b"other project");
    assert!(
        everything.iter().any(|i| i.key == b_key),
        "All includes other projects"
    );
    assert!(everything.len() >= 4);

    assert_eq!(collect(Scope::Project(project())).await, Vec::new());
}

/// Bigger than one object-store part, so the multipart path runs too.
async fn a_large_blob_streams_through<S: BlobStore + ?Sized>(store: &S) {
    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 17 * 16;
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let chunks: Vec<Bytes> = (0..CHUNKS)
        .map(|_| {
            (0..CHUNK)
                .map(|_| {
                    // xorshift64: deterministic, incompressible filler.
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state.to_le_bytes()[0]
                })
                .collect::<Vec<u8>>()
                .into()
        })
        .collect();
    let whole: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let key = key_of(project(), &whole);
    let streamed: ByteStream<'static> = stream::iter(chunks.into_iter().map(Ok)).boxed();
    store.put(&key, whole.len() as u64, streamed).await.unwrap();
    assert!(
        read_all(store, &key).await == whole,
        "17 MiB round-trips intact"
    );
    let middle = store.get(&key, Some(9_000_000..9_000_010)).await.unwrap();
    let got: Vec<u8> = middle.body.map(|c| c.unwrap().to_vec()).concat().await;
    assert_eq!(got, whole[9_000_000..9_000_010]);
}
