//! Completed private objects. Browser authorization remains exclusively in App's fresh metadata/RLS checks.
use crate::{Failure, Result};
use axum::{
    body::Body,
    http::{HeaderMap, Response, header},
};
use futures_util::{StreamExt, TryStreamExt};
use object_store::{
    Attribute, Attributes, ClientOptions, GetOptions, GetRange, ObjectStore, ObjectStoreExt,
    aws::AmazonS3Builder, buffered::BufWriter, local::LocalFileSystem, path::Path as ObjectPath,
};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;
fn unavailable() -> Failure {
    Failure(503, "Private object storage is unavailable")
}
#[derive(Clone)]
pub struct Objects {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    remote: bool,
}
#[derive(Default)]
pub struct ObjectUsage {
    pub bytes: u64,
    pub count: u64,
}
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub allow_http: bool,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub token: Option<String>,
}
impl S3Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            bucket: std::env::var("CBGPU_MATERIALS_S3_BUCKET").map_err(|_| unavailable())?,
            region: std::env::var("CBGPU_MATERIALS_S3_REGION").map_err(|_| unavailable())?,
            prefix: std::env::var("CBGPU_MATERIALS_S3_PREFIX").map_err(|_| unavailable())?,
            endpoint: std::env::var("CBGPU_MATERIALS_S3_ENDPOINT").ok(),
            allow_http: std::env::var("CBGPU_MATERIALS_S3_ALLOW_HTTP").as_deref() == Ok("1"),
            access_key: None,
            secret_key: None,
            token: None,
        })
    }
}
impl Objects {
    pub fn local(root: PathBuf) -> Result<Self> {
        Ok(Self {
            store: Arc::new(LocalFileSystem::new_with_prefix(root).map_err(|_| unavailable())?),
            prefix: String::new(),
            remote: false,
        })
    }
    pub fn s3(config: S3Config) -> Result<Self> {
        if config.bucket.len() < 3
            || config.bucket.len() > 63
            || !config
                .bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
            || config.region.is_empty()
            || config.region.len() > 64
            || !config
                .region
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || config.prefix.is_empty()
            || config.prefix.len() > 200
            || config.prefix.starts_with('/')
            || config.prefix.ends_with('/')
            || config.prefix.split('/').any(|p| {
                p.is_empty()
                    || !p
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
        {
            return Err(Failure(503, "Invalid private S3 configuration"));
        }
        if let Some(endpoint) = &config.endpoint {
            let url = reqwest::Url::parse(endpoint).map_err(|_| unavailable())?;
            let local = url.host_str().is_some_and(|h| {
                h == "localhost"
                    || h.parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
                    || h.ends_with(".internal")
                    || !h.contains('.')
            });
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.path() != "/"
                || !(url.scheme() == "https"
                    || url.scheme() == "http" && config.allow_http && local)
            {
                return Err(Failure(
                    503,
                    "S3 endpoint requires HTTPS or explicit private HTTP",
                ));
            }
        }
        let options = ClientOptions::new()
            .with_allow_http(config.allow_http)
            .with_allow_invalid_certificates(false)
            .with_connect_timeout(Duration::from_secs(5))
            .with_timeout(Duration::from_secs(120));
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(config.bucket)
            .with_region(config.region)
            .with_client_options(options)
            .with_virtual_hosted_style_request(false);
        if let Some(endpoint) = config.endpoint {
            builder = builder.with_endpoint(endpoint)
        }
        if config.access_key.is_some() != config.secret_key.is_some() {
            return Err(unavailable());
        }
        if let Some(value) = config.access_key {
            builder = builder.with_access_key_id(value)
        }
        if let Some(value) = config.secret_key {
            builder = builder.with_secret_access_key(value)
        }
        if let Some(value) = config.token {
            builder = builder.with_token(value)
        }
        Ok(Self {
            store: Arc::new(builder.build().map_err(|_| unavailable())?),
            prefix: config.prefix,
            remote: true,
        })
    }
    pub fn remote(&self) -> bool {
        self.remote
    }
    pub fn key(&self, id: Uuid) -> ObjectPath {
        let hex = id.simple().to_string();
        ObjectPath::from(if self.prefix.is_empty() {
            format!("{}/{hex}", &hex[..2])
        } else {
            format!("{}/{}/{hex}", self.prefix, &hex[..2])
        })
    }
    pub async fn size(&self, id: Uuid) -> Result<Option<u64>> {
        match self.store.head(&self.key(id)).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(_) => Err(unavailable()),
        }
    }
    /// Full read-back digest for migration verification. ETags and user metadata are not byte proofs.
    pub async fn sha256(&self, id: Uuid) -> Result<String> {
        let object = self
            .store
            .get(&self.key(id))
            .await
            .map_err(|_| unavailable())?;
        let mut stream = object.into_stream();
        let mut digest = Sha256::new();
        while let Some(bytes) = stream.try_next().await.map_err(|_| unavailable())? {
            digest.update(bytes);
        }
        Ok(format!("{:x}", digest.finalize()))
    }
    pub async fn delete(&self, id: Uuid) -> Result<()> {
        match self.store.delete(&self.key(id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(_) => Err(unavailable()),
        }
    }
    pub async fn usage(&self) -> Result<ObjectUsage> {
        let prefix = (!self.prefix.is_empty()).then(|| ObjectPath::from(self.prefix.clone()));
        let mut stream = self.store.list(prefix.as_ref());
        let mut usage = ObjectUsage::default();
        while let Some(object) = stream.try_next().await.map_err(|_| unavailable())? {
            let full = object.location.as_ref();
            let relative = if self.prefix.is_empty() {
                full
            } else {
                let Some(value) = full.strip_prefix(&(self.prefix.clone() + "/")) else {
                    continue;
                };
                value
            };
            let mut parts = relative.split('/');
            let (shard, id) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            if parts.next().is_some()
                || id.len() != 32
                || !id.bytes().all(|b| {
                    b.is_ascii_hexdigit() && (!b.is_ascii_alphabetic() || b.is_ascii_lowercase())
                })
                || shard != &id[..2]
            {
                continue;
            }
            usage.bytes = usage
                .bytes
                .checked_add(object.size)
                .ok_or_else(unavailable)?;
            usage.count += 1;
        }
        Ok(usage)
    }
    pub async fn probe(&self) -> Result<()> {
        let prefix = ObjectPath::from(self.prefix.clone());
        self.store
            .list(Some(&prefix))
            .take(1)
            .try_collect::<Vec<_>>()
            .await
            .map_err(|_| unavailable())?;
        Ok(())
    }
    pub async fn put_file(&self, id: Uuid, path: &Path, expected: u64) -> Result<()> {
        if let Some(size) = self.size(id).await? {
            return if size == expected {
                Ok(())
            } else {
                Err(Failure(409, "Stored object size differs; delete and retry"))
            };
        }
        let source = path.to_path_buf();
        let digest = tokio::task::spawn_blocking(move || -> Result<String> {
            let mut file = std::fs::File::open(source).map_err(|_| unavailable())?;
            if !file.metadata().map_err(|_| unavailable())?.is_file() {
                return Err(unavailable());
            }
            let mut hash = Sha256::new();
            let mut bytes = [0; 65536];
            loop {
                let n = file.read(&mut bytes).map_err(|_| unavailable())?;
                if n == 0 {
                    break;
                }
                hash.update(&bytes[..n]);
            }
            Ok(format!("{:x}", hash.finalize()))
        })
        .await
        .map_err(|_| unavailable())??;
        let mut attributes = Attributes::new();
        attributes.insert(Attribute::ContentType, "application/octet-stream".into());
        attributes.insert(Attribute::Metadata("cbgpu-sha256".into()), digest.into());
        let mut writer =
            BufWriter::with_capacity(self.store.clone(), self.key(id), 8 * 1024 * 1024)
                .with_max_concurrency(2);
        if self.remote {
            writer = writer.with_attributes(attributes);
        }
        let transfer = async {
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(|_| unavailable())?;
            let mut total = 0;
            loop {
                let mut bytes = vec![0; 1024 * 1024];
                let read = file.read(&mut bytes).await.map_err(|_| unavailable())?;
                if read == 0 {
                    break;
                }
                total += read as u64;
                if total > expected {
                    return Err(Failure(409, "Partial upload size changed"));
                }
                bytes.truncate(read);
                writer.put(bytes.into()).await.map_err(|_| unavailable())?;
            }
            if total != expected {
                return Err(Failure(409, "Partial upload size changed"));
            }
            Ok(())
        }
        .await;
        if transfer.is_err() {
            let _ = writer.abort().await;
        }
        transfer?;
        // shutdown consumes the multipart upload; abort afterwards would panic.
        writer.shutdown().await.map_err(|_| unavailable())?;
        if self.size(id).await? != Some(expected) {
            return Err(unavailable());
        }
        Ok(())
    }
    pub async fn response(
        &self,
        id: Uuid,
        expected: u64,
        headers: &HeaderMap,
        head: bool,
    ) -> Result<Response<Body>> {
        // If-Range requires a current validator before deciding whether to send a partial body.
        let range_allowed = if let Some(validator) = headers.get(header::IF_RANGE) {
            let meta = self
                .store
                .head(&self.key(id))
                .await
                .map_err(|_| unavailable())?;
            let value = validator
                .to_str()
                .map_err(|_| Failure(400, "Invalid conditional header"))?;
            if value.starts_with('"') {
                meta.e_tag
                    .as_deref()
                    .is_some_and(|tag| quote_etag(tag) == value)
            } else {
                httpdate::parse_http_date(value)
                    .is_ok_and(|date| std::time::SystemTime::from(meta.last_modified) <= date)
            }
        } else {
            true
        };
        let range = if head || !range_allowed {
            None
        } else {
            match parse_range(
                headers
                    .get(header::RANGE)
                    .map(|v| v.to_str().map_err(|_| Failure(416, "Invalid byte range")))
                    .transpose()?,
                expected,
            ) {
                Ok(range) => range,
                Err(Failure(416, _)) => {
                    return Response::builder()
                        .status(416)
                        .header(header::CONTENT_RANGE, format!("bytes */{expected}"))
                        .header(header::ACCEPT_RANGES, "bytes")
                        .body(Body::empty())
                        .map_err(|_| unavailable());
                }
                Err(error) => return Err(error),
            }
        };
        let mut options = GetOptions {
            range: range.clone(),
            head,
            ..Default::default()
        };
        for (name, target) in [
            (header::IF_MATCH, &mut options.if_match),
            (header::IF_NONE_MATCH, &mut options.if_none_match),
        ] {
            if let Some(value) = headers.get(name) {
                *target = Some(
                    value
                        .to_str()
                        .map_err(|_| Failure(400, "Invalid conditional header"))?
                        .to_owned(),
                );
            }
        }
        for (name, target) in [
            (header::IF_MODIFIED_SINCE, &mut options.if_modified_since),
            (
                header::IF_UNMODIFIED_SINCE,
                &mut options.if_unmodified_since,
            ),
        ] {
            if let Some(value) = headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| httpdate::parse_http_date(v).ok())
            {
                *target = Some(value.into());
            }
        }
        let object = match self.store.get_opts(&self.key(id), options).await {
            Ok(result) => result,
            Err(object_store::Error::NotModified { .. }) => {
                return Ok(Response::builder().status(304).body(Body::empty()).unwrap());
            }
            Err(object_store::Error::Precondition { .. }) => {
                return Err(Failure(412, "File precondition failed"));
            }
            Err(object_store::Error::NotFound { .. }) => {
                return Err(Failure(404, "File not found"));
            }
            Err(_) => return Err(unavailable()),
        };
        if object.meta.size != expected {
            return Err(Failure(409, "Stored size differs from authorized metadata"));
        }
        let mut response = Response::builder()
            .status(if range.is_some() { 206 } else { 200 })
            .header(header::ACCEPT_RANGES, "bytes")
            .header(
                header::CONTENT_LENGTH,
                if head {
                    expected
                } else {
                    object.range.end - object.range.start
                },
            )
            .header(
                header::LAST_MODIFIED,
                httpdate::fmt_http_date(object.meta.last_modified.into()),
            );
        if let Some(etag) = &object.meta.e_tag {
            response = response.header(header::ETAG, quote_etag(etag))
        }
        if range.is_some() {
            response = response.header(
                header::CONTENT_RANGE,
                format!(
                    "bytes {}-{}/{}",
                    object.range.start,
                    object.range.end - 1,
                    expected
                ),
            )
        }
        response
            .body(if head {
                Body::empty()
            } else {
                Body::from_stream(object.into_stream())
            })
            .map_err(|_| unavailable())
    }
}
fn quote_etag(value: &str) -> String {
    if value.starts_with('"') || value.starts_with("W/\"") {
        value.to_owned()
    } else {
        format!("\"{value}\"")
    }
}
pub fn parse_range(value: Option<&str>, size: u64) -> Result<Option<GetRange>> {
    let Some(value) = value else { return Ok(None) };
    let value = value
        .strip_prefix("bytes=")
        .ok_or(Failure(416, "Invalid byte range"))?;
    if value.contains(',') {
        return Err(Failure(416, "Multiple ranges are not supported"));
    }
    let (start, end) = value
        .split_once('-')
        .ok_or(Failure(416, "Invalid byte range"))?;
    let range = if start.is_empty() {
        let suffix = end
            .parse::<u64>()
            .map_err(|_| Failure(416, "Invalid byte range"))?;
        if suffix == 0 {
            return Err(Failure(416, "Invalid byte range"));
        }
        GetRange::Suffix(suffix)
    } else {
        let start = start
            .parse::<u64>()
            .map_err(|_| Failure(416, "Invalid byte range"))?;
        if start >= size {
            return Err(Failure(416, "Range starts past end of file"));
        }
        if end.is_empty() {
            GetRange::Offset(start)
        } else {
            let end = end
                .parse::<u64>()
                .map_err(|_| Failure(416, "Invalid byte range"))?;
            if end < start {
                return Err(Failure(416, "Invalid byte range"));
            }
            GetRange::Bounded(start..end.saturating_add(1).min(size))
        }
    };
    Ok(Some(range))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    #[test]
    fn range_validation_and_bounds() {
        assert!(parse_range(Some("bytes=1-3"), 6).is_ok());
        assert!(parse_range(Some("bytes=-3"), 6).is_ok());
        for value in [
            "bytes=6-",
            "bytes=-0",
            "bytes=3-1",
            "bytes=0-1,3-4",
            "other=0-1",
            "bytes=x-2",
        ] {
            assert_eq!(parse_range(Some(value), 6).unwrap_err().0, 416);
        }
    }
    #[tokio::test]
    async fn common_local_backend_streams_hashes_ranges_and_deletes() {
        let root = tempfile::tempdir().unwrap();
        let objects = Objects::local(root.path().into()).unwrap();
        let source = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(source.path(), b"abcdef").unwrap();
        let id = Uuid::new_v4();
        objects.put_file(id, source.path(), 6).await.unwrap();
        assert_eq!(objects.size(id).await.unwrap(), Some(6));
        assert_eq!(objects.usage().await.unwrap().bytes, 6);
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=1-3".parse().unwrap());
        let result = objects.response(id, 6, &headers, false).await.unwrap();
        assert_eq!(result.status(), 206);
        assert_eq!(result.headers()[header::CONTENT_RANGE], "bytes 1-3/6");
        assert_eq!(
            result.into_body().collect().await.unwrap().to_bytes(),
            "bcd"
        );
        assert_eq!(
            objects
                .response(id, 7, &HeaderMap::new(), false)
                .await
                .unwrap_err()
                .0,
            409
        );
        objects.delete(id).await.unwrap();
        assert_eq!(objects.size(id).await.unwrap(), None);
    }
    #[test]
    fn refuses_unsafe_configuration_before_network() {
        for endpoint in [
            "http://s3.amazonaws.com",
            "https://user:password@example.com",
            "https://example.com/path",
            "https://example.com/?secret=yes",
        ] {
            assert!(
                Objects::s3(S3Config {
                    bucket: "fixture-private".into(),
                    region: "us-east-1".into(),
                    prefix: "fixture".into(),
                    endpoint: Some(endpoint.into()),
                    allow_http: true,
                    access_key: Some("fixture".into()),
                    secret_key: Some("fixture-password".into()),
                    token: None
                })
                .is_err()
            );
        }
    }
}
