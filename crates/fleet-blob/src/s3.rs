// SPDX-License-Identifier: AGPL-3.0-only
//! S3-compatible backend over `object_store`.
//!
//! Only the generic S3 API is used (I8): path-style addressing, a configured endpoint, region and
//! static credentials. Garage is the reference store and `SeaweedFS` the documented alternative;
//! AWS itself works the same way but nothing here is specific to it.
//!
//! Small blobs (up to [`SINGLE_PUT_MAX`]) are buffered, checked, then written with one `PUT`.
//! Larger ones go up as a multipart upload that is aborted if the digest or size is wrong, so a
//! bad body never becomes visible.

use std::{io, ops::Range, sync::Arc};

use bytes::BytesMut;
use futures_util::{StreamExt, stream::BoxStream};
use object_store::{
    GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart,
    aws::AmazonS3Builder, path::Path,
};
use time::OffsetDateTime;

use crate::{
    BlobError, BlobInfo, BlobKey, BlobRead, BlobStore, ByteStream, PutOutcome, Scope, check::Check,
    store::resolve_range,
};

/// Largest blob written with a single `PUT`. Also the memory a `put` may hold.
const SINGLE_PUT_MAX: u64 = 8 * 1024 * 1024;
/// Multipart part size (S3 requires at least 5 MiB for every part but the last).
const PART_SIZE: usize = 8 * 1024 * 1024;
/// Parts uploading at once.
const PART_CONCURRENCY: usize = 4;

/// Where and how to reach an S3-compatible bucket.
#[derive(Clone)]
pub struct S3Config {
    /// Base URL of the S3 API, for example `http://garage:3900`. `http://` is accepted: the
    /// reference deployment reaches Garage over the private Compose network.
    pub endpoint: String,
    /// Signing region (Garage's default is `garage`).
    pub region: String,
    /// The bucket, which must already exist.
    pub bucket: String,
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

/// Blobs in an S3-compatible bucket.
#[derive(Clone)]
pub struct S3 {
    store: Arc<dyn ObjectStore>,
    bucket: String,
}

impl std::fmt::Debug for S3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3")
            .field("bucket", &self.bucket)
            .finish_non_exhaustive()
    }
}

impl S3 {
    /// Builds a client. No request is made: a wrong endpoint or credential shows up on first use.
    pub fn new(config: &S3Config) -> Result<Self, BlobError> {
        // reqwest's rustls has no built-in provider here; Fleet standardises on ring.
        let _ = rustls::crypto::ring::default_provider().install_default();
        for (name, value) in [
            ("endpoint", &config.endpoint),
            ("region", &config.region),
            ("bucket", &config.bucket),
            ("access_key_id", &config.access_key_id),
            ("secret_access_key", &config.secret_access_key),
        ] {
            if value.trim().is_empty() {
                return Err(BlobError::Config(format!("s3 {name} is empty")));
            }
        }
        let store = AmazonS3Builder::new()
            .with_endpoint(&config.endpoint)
            .with_region(&config.region)
            .with_bucket_name(&config.bucket)
            .with_access_key_id(&config.access_key_id)
            .with_secret_access_key(&config.secret_access_key)
            .with_virtual_hosted_style_request(false)
            .with_allow_http(config.endpoint.starts_with("http://"))
            .build()
            .map_err(|e| BlobError::Config(format!("s3: {e}")))?;
        Ok(Self {
            store: Arc::new(store),
            bucket: config.bucket.clone(),
        })
    }
}

fn path(key: &BlobKey) -> Path {
    Path::from(key.path())
}

fn backend(context: &'static str) -> impl FnOnce(object_store::Error) -> BlobError {
    move |e| BlobError::backend(context, e)
}

/// `NotFound` becomes `None`; every other failure is a backend error.
fn found<T>(
    result: object_store::Result<T>,
    context: &'static str,
) -> Result<Option<T>, BlobError> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(backend(context)(e)),
    }
}

fn info(key: BlobKey, meta: &object_store::ObjectMeta) -> BlobInfo {
    let nanos = i128::from(meta.last_modified.timestamp()) * 1_000_000_000
        + i128::from(meta.last_modified.timestamp_subsec_nanos());
    BlobInfo {
        key,
        size: meta.size,
        last_modified: OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH),
    }
}

async fn drain(mut body: ByteStream<'_>, check: &mut Check) -> Result<(), BlobError> {
    while let Some(chunk) = body.next().await {
        check.feed(&chunk.map_err(BlobError::Body)?)?;
    }
    Ok(())
}

async fn feed_parts(
    upload: &mut WriteMultipart,
    mut body: ByteStream<'_>,
    check: &mut Check,
) -> Result<(), BlobError> {
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(BlobError::Body)?;
        check.feed(&chunk)?;
        upload
            .wait_for_capacity(PART_CONCURRENCY)
            .await
            .map_err(backend("uploading a part"))?;
        upload.put(chunk);
    }
    Ok(())
}

#[async_trait::async_trait]
impl BlobStore for S3 {
    async fn put(
        &self,
        key: &BlobKey,
        size: u64,
        mut body: ByteStream<'_>,
    ) -> Result<PutOutcome, BlobError> {
        let location = path(key);
        let mut check = Check::new(key, size);
        if found(self.store.head(&location).await, "checking for the blob")?.is_some() {
            // Still prove the caller has these bytes; just don't upload them again.
            drain(body, &mut check).await?;
            check.finish()?;
            return Ok(PutOutcome::AlreadyExists);
        }

        if size <= SINGLE_PUT_MAX {
            let mut buf = BytesMut::new();
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(BlobError::Body)?;
                check.feed(&chunk)?;
                buf.extend_from_slice(&chunk);
            }
            check.finish()?;
            self.store
                .put(&location, PutPayload::from(buf.freeze()))
                .await
                .map_err(backend("uploading the blob"))?;
            return Ok(PutOutcome::Created);
        }

        let upload = self
            .store
            .put_multipart(&location)
            .await
            .map_err(backend("starting the upload"))?;
        let mut upload = WriteMultipart::new_with_chunk_size(upload, PART_SIZE);
        let checked = match feed_parts(&mut upload, body, &mut check).await {
            Ok(()) => check.finish(),
            Err(e) => Err(e),
        };
        match checked {
            Ok(()) => {
                upload
                    .finish()
                    .await
                    .map_err(backend("completing the upload"))?;
                Ok(PutOutcome::Created)
            }
            Err(e) => {
                if let Err(abort) = upload.abort().await {
                    tracing::warn!(error = %abort, "could not abort a rejected multipart upload");
                }
                Err(e)
            }
        }
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<BlobRead, BlobError> {
        let location = path(key);
        let range = match range {
            None => None,
            Some(range) => {
                let meta = found(self.store.head(&location).await, "reading the blob")?
                    .ok_or(BlobError::NotFound)?;
                Some(resolve_range(meta.size, Some(range))?)
            }
        };
        let options = GetOptions {
            range: range.map(GetRange::Bounded),
            ..GetOptions::default()
        };
        let result = found(
            self.store.get_opts(&location, options).await,
            "reading the blob",
        )?
        .ok_or(BlobError::NotFound)?;
        let (size, range) = (result.meta.size, result.range.clone());
        let body = result
            .into_stream()
            .map(|chunk| chunk.map_err(io::Error::other))
            .boxed();
        Ok(BlobRead { size, range, body })
    }

    async fn head(&self, key: &BlobKey) -> Result<Option<BlobInfo>, BlobError> {
        Ok(
            found(self.store.head(&path(key)).await, "reading the blob")?
                .map(|meta| info(*key, &meta)),
        )
    }

    async fn delete(&self, key: &BlobKey) -> Result<(), BlobError> {
        found(self.store.delete(&path(key)).await, "deleting the blob").map(|_| ())
    }

    fn list(&self, scope: Scope) -> BoxStream<'_, Result<BlobInfo, BlobError>> {
        let prefix = match scope {
            Scope::All => None,
            Scope::Project(id) => Some(Path::from(id.hyphenated().to_string())),
        };
        self.store
            .list(prefix.as_ref())
            .filter_map(|item| async move {
                match item {
                    Err(e) => Some(Err(backend("listing blobs")(e))),
                    Ok(meta) => {
                        let key = BlobKey::from_path(meta.location.as_ref());
                        if key.is_none() {
                            tracing::debug!(object = %meta.location, "skipping an object that is not a blob");
                        }
                        key.map(|key| Ok(info(key, &meta)))
                    }
                }
            })
            .boxed()
    }
}
