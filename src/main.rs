use axum::http::{HeaderValue, Method, header};
use cbgpu_materials::{App, storage::Storage};
use std::{env, net::SocketAddr, path::PathBuf, time::Duration};
use tower_http::cors::CorsLayer;
fn scope_flag(value: &str) -> std::result::Result<bool, &'static str> {
    match value {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err("Invalid server authorization flag"),
    }
}
fn scoped_mode() -> std::result::Result<bool, &'static str> {
    let mut enabled = false;
    for key in ["CBGPU_MULTISCHOOL_V1", "CBGPU_ACCOUNT_V2"] {
        match env::var(key) {
            Err(env::VarError::NotPresent) => {}
            Ok(v) => enabled |= scope_flag(&v)?,
            _ => return Err("Invalid server authorization flag"),
        }
    }
    Ok(enabled)
}
async fn run() -> std::result::Result<(), &'static str> {
    if env::args().nth(1).as_deref() == Some("healthcheck") {
        let port = env::var("CBGPU_MATERIALS_LISTEN")
            .unwrap_or_else(|_| "127.0.0.1:8423".into())
            .parse::<SocketAddr>()
            .map_err(|_| "Invalid listen address")?
            .port();
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(7))
            .build()
            .map_err(|_| "Cannot initialize health client")?
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .map_err(|_| "Materials readiness failed")?;
        if !response.status().is_success() {
            return Err("Materials readiness failed");
        }
        return Ok(());
    }
    let root = PathBuf::from(
        env::var("CBGPU_MATERIALS_DIR").unwrap_or_else(|_| "/home/cbgpu/cbgpu-materials".into()),
    );
    let quota = env::var("CBGPU_MATERIALS_QUOTA_BYTES")
        .unwrap_or_else(|_| (100_u64 * 1024 * 1024 * 1024).to_string())
        .parse()
        .map_err(|_| "Invalid storage quota")?;
    let mut storage = Storage::open(root, quota).map_err(
        |_| "Materials storage is not ready or is already owned by another Rust instance",
    )?;
    match env::var("CBGPU_MATERIALS_STORAGE_BACKEND").as_deref() {
        Err(_) | Ok("local") => {}
        Ok("s3") => {
            if env::var("CBGPU_MATERIALS_WRITES_ENABLED").as_deref() == Ok("1")
                && env::var("CBGPU_MATERIALS_S3_SINGLE_WRITER").as_deref() != Ok("1")
            {
                return Err(
                    "S3 writes require reviewed single-writer staging; multi-replica coordination is not enabled",
                );
            }
            storage = storage.with_objects(
                cbgpu_materials::objects::Objects::s3(
                    cbgpu_materials::objects::S3Config::from_env()
                        .map_err(|_| "Private S3 configuration required")?,
                )
                .map_err(|_| "Private S3 configuration invalid")?,
            );
        }
        _ => return Err("Storage backend must be local or s3"),
    }
    let writes = match env::var("CBGPU_MATERIALS_WRITES_ENABLED").as_deref() {
        Ok("1") => true,
        Ok("0") | Err(_) => false,
        _ => return Err("CBGPU_MATERIALS_WRITES_ENABLED must be 0 or 1"),
    };
    if writes && env::var("CBGPU_MATERIALS_LEGACY_QUIESCED").as_deref() != Ok("1") {
        return Err(
            "Drain the legacy writer and set CBGPU_MATERIALS_LEGACY_QUIESCED=1 before enabling writes",
        );
    }
    let app = App::new(
        storage.clone(),
        env::var("CBGPU_SUPABASE_URL").map_err(|_| "Metadata origin is required")?,
        env::var("CBGPU_SUPABASE_ANON_KEY").map_err(|_| "Metadata public key is required")?,
        writes,
    )
    .map_err(|_| "Metadata configuration is invalid")?
    .with_scoped_mode(scoped_mode()?);
    let origins = env::var("CBGPU_MATERIALS_ORIGINS")
        .unwrap_or_else(|_| {
            "https://cbgpu.com,https://admin.cbgpu.com,http://localhost:3000".into()
        })
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| HeaderValue::from_str(s.trim()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| "Invalid CORS origin")?;
    let cors = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            header::ACCEPT_LANGUAGE,
            header::CONTENT_LANGUAGE,
            header::HeaderName::from_static("x-cbgpu-school-id"),
        ]);
    let listen: SocketAddr = env::var("CBGPU_MATERIALS_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8423".into())
        .parse()
        .map_err(|_| "Invalid listen address")?;
    if listen.port() < 1024
        || (!listen.ip().is_loopback()
            && env::var("CBGPU_MATERIALS_CONTAINER_BIND").as_deref() != Ok("1"))
    {
        return Err("Use loopback or explicitly opt into an isolated container bind");
    }
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|_| "Materials listen failed")?;
    let cleanup = if writes {
        Some(tokio::spawn(async move {
            loop {
                let storage = storage.clone();
                let _ = storage.cleanup_object_deletions().await;
                let _ = storage.usage_async(true).await;
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
        }))
    } else {
        None
    };
    println!("CBGPU Rust materials listening on {listen}; writes enabled={writes}");
    let result = axum::serve(listener, cbgpu_materials::router(app).layer(cors))
        .with_graceful_shutdown(async {
            #[cfg(unix)]
            {
                if let Ok(mut terminate) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
                } else {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        })
        .await;
    if let Some(cleanup) = cleanup {
        cleanup.abort();
    }
    result.map_err(|_| "Materials server failed")
}
#[tokio::main(worker_threads = 4)]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scoped_flags_have_no_truthy_or_malformed_legacy_fallback() {
        for value in ["1", "true"] {
            assert!(super::scope_flag(value).unwrap());
        }
        for value in ["0", "false"] {
            assert!(!super::scope_flag(value).unwrap());
        }
        for value in ["", "yes", "TRUE", " 1", "1 ", "tru"] {
            assert!(super::scope_flag(value).is_err());
        }
    }
}
