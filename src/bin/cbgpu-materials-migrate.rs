use cbgpu_materials::{
    Failure,
    migration::{self, Manifest},
    objects::{Objects, S3Config},
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf};
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!(
            "Migration refused: {} ({}). Sources were not deleted; rerun with a NEW report path after correcting the input.",
            error.1, error.0
        );
        std::process::exit(1);
    }
}
async fn run() -> cbgpu_materials::Result<()> {
    let mut values = BTreeMap::new();
    let mut apply = false;
    let mut fenced = false;
    let mut resume = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--apply" => {
                if apply {
                    return Err(Failure(400, "Duplicate apply flag"));
                }
                apply = true;
            }
            "--confirm-writers-fenced" => fenced = true,
            "--resume" => resume = true,
            "--direction"
            | "--manifest"
            | "--expected-manifest-sha256"
            | "--local-root"
            | "--report"
            | "--prefix" => {
                let value = args.next().ok_or(Failure(400, "Missing argument"))?;
                if values.insert(arg, value).is_some() {
                    return Err(Failure(400, "Duplicate option"));
                }
            }
            _ => {
                return Err(Failure(
                    400,
                    "Use --direction local-to-s3|s3-to-local --manifest FILE --local-root ROOT --report NEWFILE [--apply --confirm-writers-fenced --expected-manifest-sha256 HEX] [--resume]",
                ));
            }
        }
    }
    if apply && !fenced {
        return Err(Failure(
            403,
            "Apply requires explicit external-writer fence acknowledgement",
        ));
    }
    let required = |name: &str| {
        values
            .get(name)
            .cloned()
            .ok_or(Failure(400, "Missing required option"))
    };
    let direction = required("--direction")?;
    let reverse = match direction.as_str() {
        "local-to-s3" => false,
        "s3-to-local" => true,
        _ => return Err(Failure(400, "Unknown direction")),
    };
    if resume && (!reverse || !apply) {
        return Err(Failure(400, "Resume is only for explicit reverse apply"));
    }
    let manifest = Manifest::read(&PathBuf::from(required("--manifest")?))?;
    if apply && required("--expected-manifest-sha256")? != manifest.sha256 {
        return Err(Failure(409, "Pinned manifest SHA-256 does not match"));
    }
    let local = PathBuf::from(required("--local-root")?);
    let report = PathBuf::from(required("--report")?);
    // Complete path validation precedes any destination creation or object mutation.
    let candidate = if reverse {
        migration::planned_destination(&local)?
    } else {
        migration::checked_root(&local)?
    };
    let report = migration::report_path(&report, &candidate)?;
    let config = S3Config {
        bucket: std::env::var("CBGPU_MATERIALS_S3_BUCKET")
            .map_err(|_| Failure(400, "S3 bucket is required"))?,
        region: std::env::var("CBGPU_MATERIALS_S3_REGION")
            .map_err(|_| Failure(400, "S3 region is required"))?,
        prefix: values
            .get("--prefix")
            .cloned()
            .or_else(|| std::env::var("CBGPU_MATERIALS_S3_PREFIX").ok())
            .unwrap_or_else(|| "materials-private-v1".into()),
        endpoint: std::env::var("CBGPU_MATERIALS_S3_ENDPOINT").ok(),
        allow_http: std::env::var("CBGPU_MATERIALS_S3_ALLOW_HTTP").as_deref() == Ok("1"),
        access_key: None,
        secret_key: None,
        token: None,
    };
    let location_sha256 = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                &config.bucket,
                &config.region,
                &config.prefix,
                &config.endpoint
            ))
            .map_err(|_| Failure(400, "Cannot encode storage location"))?
        )
    );
    let objects = Objects::s3(config)?;
    let root = if reverse && apply {
        migration::new_destination(&candidate, &manifest, resume)?
    } else {
        candidate
    };
    let result = migration::copy_inventory(&objects, &manifest, &root, reverse, apply).await;
    match result {
        Ok(mut value) => {
            value.object_location_sha256 = Some(location_sha256.clone());
            migration::write_report(&report, &value, &root)?;
            println!(
                "{} objects verified; manifest SHA-256 {}. External writers were NOT inspected by this tool.",
                value.items.len(),
                manifest.sha256
            );
            Ok(())
        }
        Err(error) => {
            let receipt = serde_json::json!({"version":1,"status":"FAILED","direction":direction,"manifest_sha256":manifest.sha256,"error":error.1,"apply":apply,"external_writers_inspected":false,"source_deletion_attempted":false,"object_location_sha256":location_sha256});
            let _ = migration::write_report(&report, &receipt, &root);
            Err(error)
        }
    }
}
