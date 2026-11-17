//! Optional trusted TLS MinIO fixture only. No AWS credentials, endpoints or real objects.
use axum::http::HeaderMap;
use cbgpu_materials::objects::{Objects, S3Config};
use http_body_util::BodyExt;
use object_store::{
    aws::{AwsAuthorizer, AwsCredential},
    client::{HttpRequest, HttpRequestBody},
};
use sha2::{Digest, Sha256};
use uuid::Uuid;
#[tokio::test]
#[ignore = "Requires a disposable HTTPS loopback MinIO and private fixture CA; never AWS"]
async fn trusted_tls_s3_write_range_read_hash_and_delete() {
    let endpoint = std::env::var("CBGPU_MATERIALS_TEST_S3_ENDPOINT").unwrap();
    let url = reqwest::Url::parse(&endpoint).unwrap();
    assert_eq!(url.scheme(), "https");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert!(url.username().is_empty() && url.password().is_none());
    let ca = std::fs::read(std::env::var("CBGPU_MATERIALS_TEST_S3_CA").unwrap()).unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .build()
        .unwrap();
    let bucket = "cbgpu-tls-fixture";
    let credential = AwsCredential {
        key_id: "materials-fixture".into(),
        secret_key: "materials-fixture-private-only".into(),
        token: None,
    };
    let mut request: HttpRequest = axum::http::Request::builder()
        .method("PUT")
        .uri(format!("{endpoint}/{bucket}"))
        .body(HttpRequestBody::empty())
        .unwrap();
    AwsAuthorizer::new(&credential, "s3", "us-east-1")
        .try_authorize(&mut request, None)
        .unwrap();
    let r = client
        .put(request.uri().to_string())
        .headers(request.headers().clone())
        .body("")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success() || r.status() == 409);
    let objects = Objects::s3(S3Config {
        bucket: bucket.into(),
        region: "us-east-1".into(),
        prefix: "trusted-tls".into(),
        endpoint: Some(endpoint),
        allow_http: false,
        access_key: Some(credential.key_id),
        secret_key: Some(credential.secret_key),
        token: None,
    })
    .unwrap();
    let id = Uuid::new_v4();
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("fixture.bin");
    std::fs::write(&input, b"abcdef").unwrap();
    if std::env::var("CBGPU_MATERIALS_TEST_S3_EXPECT_UNTRUSTED").as_deref() == Ok("1") {
        assert!(
            objects.put_file(id, &input, 6).await.is_err(),
            "untrusted self-signed endpoint must never be accepted"
        );
        return;
    }
    objects.put_file(id, &input, 6).await.unwrap();
    assert_eq!(objects.size(id).await.unwrap(), Some(6));
    assert_eq!(
        objects.sha256(id).await.unwrap(),
        format!("{:x}", Sha256::digest(b"abcdef"))
    );
    let mut headers = HeaderMap::new();
    headers.insert("range", "bytes=1-3".parse().unwrap());
    let r = objects.response(id, 6, &headers, false).await.unwrap();
    assert_eq!(r.status(), 206);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"bcd");
    objects.delete(id).await.unwrap();
    assert_eq!(objects.size(id).await.unwrap(), None);
}
