use axum::{
    Json, Router,
    extract::{Request, State},
    response::IntoResponse,
};
use cbgpu_materials::{App, router, storage::Storage};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Clone)]
struct Metadata {
    row: Arc<Mutex<Value>>,
    fail_ack: Arc<Mutex<bool>>,
}
async fn metadata(State(state): State<Metadata>, request: Request) -> axum::response::Response {
    let token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if request.uri().path() == "/auth/v1/user" {
        return if token == "Bearer owner" {
            Json(json!({"id":"owner"})).into_response()
        } else if token == "Bearer other" {
            Json(json!({"id":"other"})).into_response()
        } else {
            (axum::http::StatusCode::UNAUTHORIZED, Json(json!({}))).into_response()
        };
    }
    if request.uri().path() == "/rest/v1/profiles" {
        return Json(json!([{"is_admin":false}])).into_response();
    }
    if request.method() == "PATCH" {
        if *state.fail_ack.lock().unwrap() {
            return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({}))).into_response();
        }
        state.row.lock().unwrap()["status"] = json!("ready");
    }
    if token == "Bearer other" {
        return Json(json!([])).into_response();
    }
    Json(json!([state.row.lock().unwrap().clone()])).into_response()
}
struct Fixture {
    _directory: tempfile::TempDir,
    app: Router,
    storage: Storage,
    id: Uuid,
    metadata: Metadata,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new(writes: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path().into(), 100).unwrap();
        let id = Uuid::new_v4();
        let state = Metadata {
            row: Arc::new(Mutex::new(
                json!({"id":id,"uploader_id":"owner","status":"pending","size_bytes":6,"file_name":"연구.txt"}),
            )),
            fail_ack: Arc::new(Mutex::new(false)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = Router::new().fallback(metadata).with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
        let app = router(
            App::new(
                storage.clone(),
                format!("http://{address}"),
                "public-anon".into(),
                writes,
            )
            .unwrap(),
        );
        Self {
            _directory: directory,
            app,
            storage,
            id,
            metadata: state,
            task,
        }
    }
    async fn call(&self, method: &str, suffix: &str, token: &str, body: &str) -> (u16, Vec<u8>) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(format!("/materials/{}{suffix}", self.id))
            .header("authorization", format!("Bearer {token}"))
            .body(axum::body::Body::from(body.to_owned()))
            .unwrap();
        let response = self.app.clone().oneshot(req).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, bytes)
    }
}
#[tokio::test]
async fn resumes_chunks_and_serves_ranges_without_leaking_private_files() {
    let f = Fixture::new(true).await;
    assert_eq!(f.call("PUT", "/chunks", "owner", "abc").await.0, 200);
    assert_eq!(
        f.call("PUT", "/chunks?offset=0", "owner", "def").await.0,
        409
    );
    assert_eq!(
        f.call("PUT", "/chunks?offset=3", "owner", "def").await.0,
        200
    );
    assert_eq!(
        f.call("GET", "/content", "owner", "").await,
        (200, b"abcdef".to_vec())
    );
    assert_eq!(f.call("GET", "/content", "other", "").await.0, 404);
    assert_eq!(f.call("GET", "/content", "invalid", "").await.0, 401);
    let response = f
        .app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/materials/{}/content", f.id))
                .header("authorization", "Bearer owner")
                .header("range", "bytes=1-3")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("filename*=utf-8''")
    );
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "bcd"
    );
}
#[tokio::test]
async fn lost_metadata_ack_preserves_bytes_and_status_request_recovers() {
    let f = Fixture::new(true).await;
    *f.metadata.fail_ack.lock().unwrap() = true;
    assert_eq!(f.call("PUT", "/content", "owner", "abcdef").await.0, 503);
    let paths = f.storage.paths(f.id);
    assert_eq!(std::fs::read(&paths.complete).unwrap(), b"abcdef");
    assert!(paths.reservation.exists());
    *f.metadata.fail_ack.lock().unwrap() = false;
    let (status, bytes) = f.call("GET", "/upload", "owner", "").await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["complete"],
        true
    );
    assert!(!paths.reservation.exists());
}
#[tokio::test]
async fn oversized_chunk_rolls_back_and_whole_failure_releases_reservation() {
    let f = Fixture::new(true).await;
    assert_eq!(f.call("PUT", "/chunks", "owner", "abc").await.0, 200);
    assert_eq!(
        f.call("PUT", "/chunks?offset=3", "owner", "defg").await.0,
        413
    );
    assert_eq!(
        std::fs::read(f.storage.paths(f.id).partial).unwrap(),
        b"abc"
    );
    let f = Fixture::new(true).await;
    assert_eq!(f.call("PUT", "/content", "owner", "abc").await.0, 400);
    assert_eq!(f.storage.usage(false).unwrap().reserved_bytes, 0);
}
#[tokio::test]
async fn shadow_never_changes_upload_state() {
    let f = Fixture::new(false).await;
    assert_eq!(f.call("PUT", "/content", "owner", "abcdef").await.0, 503);
    assert_eq!(f.call("GET", "/upload", "owner", "").await.0, 503);
    assert_eq!(f.call("DELETE", "", "owner", "").await.0, 503);
    assert!(!f.storage.paths(f.id).reservation.exists());
}
#[test]
fn reservations_survive_restart_and_enforce_quota_and_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let id = Uuid::new_v4();
    {
        let storage = Storage::open(directory.path().into(), 10).unwrap();
        storage.reserve(id, "owner", 8).unwrap();
        assert!(Storage::open(directory.path().into(), 10).is_err());
    }
    let storage = Storage::open(directory.path().into(), 10).unwrap();
    assert_eq!(storage.usage(false).unwrap().remaining_bytes, 2);
    assert_eq!(storage.reserve(id, "other", 8).unwrap_err().0, 409);
    assert_eq!(
        storage.reserve(Uuid::new_v4(), "owner", 3).unwrap_err().0,
        507
    );
    storage.delete(id).unwrap();
    assert_eq!(storage.usage(false).unwrap().remaining_bytes, 10);
}
#[cfg(unix)]
#[test]
fn refuses_symlinked_storage_shards() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path().into(), 100).unwrap();
    let id = Uuid::new_v4();
    std::os::unix::fs::symlink(outside.path(), storage.paths(id).complete.parent().unwrap())
        .unwrap();
    assert!(storage.reserve(id, "owner", 6).is_err());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn concurrent_reservations_cannot_overcommit_quota() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path().into(), 10).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers = (0..2)
        .map(|_| {
            let storage = storage.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                storage.reserve(Uuid::new_v4(), "owner", 8).is_ok()
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    assert_eq!(
        workers
            .into_iter()
            .filter_map(|w| w.join().ok())
            .filter(|ok| *ok)
            .count(),
        1
    );
    assert_eq!(storage.usage(false).unwrap().reserved_bytes, 8);
}

#[tokio::test]
async fn cancelled_request_truncates_uncommitted_chunk() {
    use futures_util::StreamExt;
    let f = Fixture::new(true).await;
    let stream = futures_util::stream::once(async {
        Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"abc"))
    })
    .chain(futures_util::stream::pending());
    let request = axum::http::Request::builder()
        .method("PUT")
        .uri(format!("/materials/{}/chunks", f.id))
        .header("authorization", "Bearer owner")
        .body(axum::body::Body::from_stream(stream))
        .unwrap();
    let task = tokio::spawn(f.app.clone().oneshot(request));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while std::fs::metadata(f.storage.paths(f.id).partial.clone())
            .map(|m| m.len())
            .unwrap_or(0)
            != 3
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        std::fs::metadata(f.storage.paths(f.id).partial)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(f.storage.usage(false).unwrap().reserved_bytes, 6);
    assert_eq!(f.call("PUT", "/chunks", "owner", "abcdef").await.0, 200);
}

#[test]
fn cleanup_skips_active_uploads_and_never_deletes_completed_files() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path().into(), 100).unwrap();
    let id = Uuid::new_v4();
    let paths = storage.reserve(id, "owner", 6).unwrap();
    std::fs::write(&paths.partial, b"abc").unwrap();
    for path in [&paths.partial, &paths.reservation] {
        std::fs::File::open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
    }
    let lock = storage.lock(id);
    let guard = lock.try_lock().unwrap();
    assert_eq!(storage.usage(true).unwrap().reserved_bytes, 6);
    assert!(paths.partial.exists());
    drop(guard);
    assert_eq!(storage.usage(true).unwrap().reserved_bytes, 0);
    assert!(!paths.partial.exists());
    assert!(!paths.reservation.exists());
    let ready = Uuid::new_v4();
    let paths = storage.reserve(ready, "owner", 6).unwrap();
    std::fs::write(&paths.partial, b"abcdef").unwrap();
    storage.promote(ready, 6).unwrap();
    storage.acknowledge(ready).unwrap();
    std::fs::File::open(&paths.complete)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
    assert_eq!(storage.usage(true).unwrap().used_bytes, 6);
    assert_eq!(std::fs::read(paths.complete).unwrap(), b"abcdef");
}
