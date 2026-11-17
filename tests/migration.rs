use cbgpu_materials::{
    migration::{self, Manifest},
    objects::Objects,
};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};
use uuid::Uuid;
fn manifest(id: Uuid, bytes: &[u8]) -> Manifest {
    Manifest::parse(format!("{id}\t{}\t{:x}\n", bytes.len(), Sha256::digest(bytes)).as_bytes())
        .unwrap()
}
fn put(root: &Path, id: Uuid, bytes: &[u8]) {
    let name = id.simple().to_string();
    fs::create_dir_all(root.join(&name[..2])).unwrap();
    fs::write(root.join(&name[..2]).join(name), bytes).unwrap();
}
#[test]
fn rejects_noncanonical_inventory_and_symlink_sources() {
    let id = Uuid::new_v4();
    let good = format!("{id}\t3\t{:x}\n", Sha256::digest(b"abc"));
    for bad in [
        good.trim_end().to_owned(),
        good.replace("\t3\t", "\t03\t"),
        format!("{good}{good}"),
        good.replace("\t3\t", "\t0\t"),
    ] {
        assert!(Manifest::parse(bad.as_bytes()).is_err());
    }
    let root = tempfile::tempdir().unwrap();
    let m = manifest(id, b"abc");
    put(root.path(), id, b"bad");
    assert!(migration::local_snapshot(root.path(), &m.entries[0]).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"abc").unwrap();
        let name = id.simple().to_string();
        let path = root.path().join(&name[..2]).join(name);
        fs::remove_file(&path).unwrap();
        symlink(outside.path(), path).unwrap();
        assert!(migration::local_snapshot(root.path(), &m.entries[0]).is_err());
    }
}
#[tokio::test]
async fn verified_copy_resume_reverse_and_no_overwrites() {
    let source = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let id = Uuid::new_v4();
    let data = b"private synthetic project";
    let m = manifest(id, data);
    put(source.path(), id, data);
    let objects = Objects::local(remote.path().into()).unwrap();
    let before = fs::read(
        source
            .path()
            .join(&id.simple().to_string()[..2])
            .join(id.simple().to_string()),
    )
    .unwrap();
    let plan = migration::copy_inventory(&objects, &m, source.path(), false, false)
        .await
        .unwrap();
    assert_eq!(plan.items[0].result, "SOURCE_VERIFIED");
    assert_eq!(objects.size(id).await.unwrap(), None);
    let copied = migration::copy_inventory(&objects, &m, source.path(), false, true)
        .await
        .unwrap();
    assert_eq!(copied.items[0].result, "COPIED_VERIFIED");
    assert!(!copied.external_writers_inspected);
    assert_eq!(
        migration::copy_inventory(&objects, &m, source.path(), false, true)
            .await
            .unwrap()
            .items[0]
            .result,
        "VERIFIED_EXISTING"
    );
    let destination = parent.path().join("new-root");
    let root = migration::new_destination(&destination, &m, false).unwrap();
    assert!(migration::new_destination(&destination, &m, false).is_err());
    migration::copy_inventory(&objects, &m, &root, true, true)
        .await
        .unwrap();
    migration::new_destination(&destination, &m, true).unwrap();
    assert_eq!(
        migration::copy_inventory(&objects, &m, &root, true, true)
            .await
            .unwrap()
            .items[0]
            .result,
        "VERIFIED_EXISTING"
    );
    assert_eq!(
        fs::read(
            root.join(&id.simple().to_string()[..2])
                .join(id.simple().to_string())
        )
        .unwrap(),
        before
    );
    let wrong = manifest(id, b"different byte size");
    assert!(
        migration::copy_inventory(&objects, &wrong, source.path(), false, true)
            .await
            .is_err()
    );
    assert!(migration::new_destination(&destination, &wrong, true).is_err());
    let report = parent.path().join("new-report.json");
    migration::write_report(&report, &copied, &root).unwrap();
    assert!(migration::write_report(&report, &copied, &root).is_err());
    assert!(migration::report_path(&root.join("report"), &root).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(report).unwrap().permissions().mode() & 0o077,
            0
        );
    }
}
#[test]
fn cli_requires_fence_before_any_copy() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cbgpu-materials-migrate"))
        .args(["--apply"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("fence acknowledgement"));
}

#[tokio::test]
#[ignore = "requires disposable loopback MinIO"]
async fn minio_full_inventory_cli_forward_reverse_and_corruption_refusal() {
    use cbgpu_materials::objects::S3Config;
    use object_store::{
        aws::{AwsAuthorizer, AwsCredential},
        client::{HttpRequest, HttpRequestBody},
    };
    let endpoint = std::env::var("CBGPU_MATERIALS_TEST_S3_ENDPOINT").unwrap();
    let parsed = reqwest::Url::parse(&endpoint).unwrap();
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    assert_eq!(parsed.scheme(), "http");
    let bucket = format!("cbgpu-migration-{}", Uuid::new_v4().simple());
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
    assert!(
        reqwest::Client::new()
            .put(request.uri().to_string())
            .headers(request.headers().clone())
            .body("")
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let objects = Objects::s3(S3Config {
        bucket: bucket.clone(),
        region: "us-east-1".into(),
        prefix: "migration".into(),
        endpoint: Some(endpoint.clone()),
        allow_http: true,
        access_key: Some(credential.key_id),
        secret_key: Some(credential.secret_key),
        token: None,
    })
    .unwrap();
    let source = tempfile::tempdir().unwrap();
    let reports = tempfile::tempdir().unwrap();
    let id = Uuid::new_v4();
    let second = Uuid::new_v4();
    let first_data = b"private synthetic bytes";
    let large = vec![17u8; 9 * 1024 * 1024];
    put(source.path(), id, first_data);
    put(source.path(), second, &large);
    let mut lines = [
        format!(
            "{id}\t{}\t{:x}\n",
            first_data.len(),
            Sha256::digest(first_data)
        ),
        format!("{second}\t{}\t{:x}\n", large.len(), Sha256::digest(&large)),
    ];
    lines.sort();
    let manifest_bytes = lines.concat();
    let m = Manifest::parse(manifest_bytes.as_bytes()).unwrap();
    let manifest_path = reports.path().join("inventory.tsv");
    fs::write(&manifest_path, &manifest_bytes).unwrap();
    let invoke = |direction: &str, root: &Path, report: &str, resume: bool| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_cbgpu-materials-migrate"));
        command
            .env_clear()
            .env("CBGPU_MATERIALS_S3_BUCKET", &bucket)
            .env("CBGPU_MATERIALS_S3_REGION", "us-east-1")
            .env("CBGPU_MATERIALS_S3_PREFIX", "migration")
            .env("CBGPU_MATERIALS_S3_ENDPOINT", &endpoint)
            .env("CBGPU_MATERIALS_S3_ALLOW_HTTP", "1")
            .env("AWS_ACCESS_KEY_ID", "materials-fixture")
            .env("AWS_SECRET_ACCESS_KEY", "materials-fixture-private-only")
            .args(["--direction", direction, "--manifest"])
            .arg(&manifest_path)
            .arg("--local-root")
            .arg(root)
            .arg("--report")
            .arg(reports.path().join(report))
            .args([
                "--apply",
                "--confirm-writers-fenced",
                "--expected-manifest-sha256",
                &m.sha256,
            ]);
        if resume {
            command.arg("--resume");
        }
        command.output().unwrap()
    };
    let forward = invoke("local-to-s3", source.path(), "forward.json", false);
    assert!(
        forward.status.success(),
        "{}",
        String::from_utf8_lossy(&forward.stderr)
    );
    assert_eq!(
        objects.sha256(second).await.unwrap(),
        format!("{:x}", Sha256::digest(&large))
    );
    let repeated = invoke("local-to-s3", source.path(), "resume-forward.json", false);
    assert!(repeated.status.success());
    let destination = reports.path().join("new-root");
    let reverse = invoke("s3-to-local", &destination, "reverse.json", false);
    assert!(
        reverse.status.success(),
        "{}",
        String::from_utf8_lossy(&reverse.stderr)
    );
    assert!(
        invoke("s3-to-local", &destination, "resume-reverse.json", true)
            .status
            .success()
    );
    assert!(
        !invoke(
            "s3-to-local",
            &destination,
            "refused-existing-root.json",
            false
        )
        .status
        .success()
    );
    for entry in &m.entries {
        migration::local_snapshot(&destination, entry).unwrap();
        migration::local_snapshot(source.path(), entry).unwrap();
    }
    // Corrupt same-length source: verified source mismatch cannot overwrite the destination.
    put(source.path(), id, &vec![0u8; first_data.len()]);
    assert!(
        !invoke("local-to-s3", source.path(), "corrupt-source.json", false)
            .status
            .success()
    );
    assert_eq!(
        objects.sha256(id).await.unwrap(),
        format!("{:x}", Sha256::digest(first_data))
    );
    // Existing different destination must never be silently replaced.
    put(source.path(), id, first_data);
    objects.delete(id).await.unwrap();
    let corrupt = tempfile::NamedTempFile::new().unwrap();
    fs::write(corrupt.path(), vec![0u8; first_data.len()]).unwrap();
    objects
        .put_file(id, corrupt.path(), first_data.len() as u64)
        .await
        .unwrap();
    assert!(
        !invoke(
            "local-to-s3",
            source.path(),
            "corrupt-destination.json",
            false
        )
        .status
        .success()
    );
    assert_eq!(
        objects.sha256(id).await.unwrap(),
        format!("{:x}", Sha256::digest(vec![0u8; first_data.len()]))
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(reports.path().join("forward.json")).unwrap()).unwrap();
    assert_eq!(report["external_writers_inspected"], false);
    assert_eq!(report["manifest_sha256"], m.sha256);
}

#[test]
fn cli_pins_manifest_and_rejects_reports_inside_source_before_storage_access() {
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let id = Uuid::new_v4();
    let bytes = format!("{id}\t3\t{:x}\n", Sha256::digest(b"abc"));
    let m = Manifest::parse(bytes.as_bytes()).unwrap();
    let manifest = private.path().join("inventory.tsv");
    fs::write(&manifest, bytes).unwrap();
    let invoke = |hash: &str, report: &Path| {
        std::process::Command::new(env!("CARGO_BIN_EXE_cbgpu-materials-migrate"))
            .env_clear()
            .args(["--direction", "local-to-s3", "--manifest"])
            .arg(&manifest)
            .arg("--local-root")
            .arg(root.path())
            .arg("--report")
            .arg(report)
            .args([
                "--apply",
                "--confirm-writers-fenced",
                "--expected-manifest-sha256",
                hash,
            ])
            .output()
            .unwrap()
    };
    let target = private.path().join("no-report.json");
    let bad = invoke(&"0".repeat(64), &target);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("Pinned manifest"));
    assert!(!target.exists());
    let inside = invoke(&m.sha256, &root.path().join("must-not-write.json"));
    assert!(!inside.status.success());
    assert!(!root.path().join("must-not-write.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let name = id.simple().to_string();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join(&name), b"abc").unwrap();
        symlink(outside.path(), root.path().join(&name[..2])).unwrap();
        assert!(migration::local_snapshot(root.path(), &m.entries[0]).is_err());
    }
}
