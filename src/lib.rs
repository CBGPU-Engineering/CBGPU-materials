pub mod objects;
pub mod storage;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    sync::Arc,
    time::Duration,
};
use storage::{MAX_CHUNK, MAX_FILE, Storage};
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Debug)]
pub struct Failure(pub u16, pub &'static str);
pub type Result<T> = std::result::Result<T, Failure>;
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (
            StatusCode::from_u16(self.0).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            axum::Json(json!({"detail":self.1})),
        )
            .into_response()
    }
}
#[derive(Clone)]
pub struct App {
    pub storage: Storage,
    pub backend: String,
    pub anon: String,
    pub writes: bool,
    client: reqwest::Client,
}
impl App {
    pub fn new(storage: Storage, backend: String, anon: String, writes: bool) -> Result<Self> {
        let url =
            reqwest::Url::parse(&backend).map_err(|_| Failure(503, "Invalid metadata origin"))?;
        if !["http", "https"].contains(&url.scheme())
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || anon.is_empty()
            || anon.starts_with("sb_secret_")
        {
            return Err(Failure(
                503,
                "Use a configured metadata origin and public anon key",
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| Failure(503, "Cannot initialize metadata transport"))?;
        Ok(Self {
            storage,
            backend: backend.trim_end_matches('/').to_owned(),
            anon,
            writes,
            client,
        })
    }
    async fn json(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut builder = self
            .client
            .request(method.clone(), format!("{}{path}", self.backend))
            .header("apikey", &self.anon)
            .bearer_auth(token)
            .query(query);
        if method == reqwest::Method::PATCH || method == reqwest::Method::DELETE {
            builder = builder.header("Prefer", "return=representation");
        }
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|_| Failure(503, "Materials database is unavailable"))?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure(503, "Materials database is unavailable"))?
        {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err(Failure(503, "Materials response is too large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, value))
    }
    async fn user(&self, token: &str) -> Result<String> {
        let (status, value) = self
            .json(reqwest::Method::GET, "/auth/v1/user", token, &[], None)
            .await?;
        if status != 200 {
            return Err(Failure(401, "Invalid session"));
        }
        value["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or(Failure(503, "Invalid account response"))
    }
    async fn material(&self, token: &str, id: Uuid) -> Result<Value> {
        let (status, value) = self
            .json(
                reqwest::Method::GET,
                "/rest/v1/shared_materials",
                token,
                &[
                    ("id", format!("eq.{id}")),
                    ("select", "*".into()),
                    ("limit", "1".into()),
                ],
                None,
            )
            .await?;
        if status != 200 {
            return Err(Failure(503, "Materials database is unavailable"));
        }
        value
            .as_array()
            .and_then(|rows| rows.first())
            .cloned()
            .ok_or(Failure(404, "Material not found"))
    }
    async fn owned(&self, token: &str, id: Uuid) -> Result<(String, Value)> {
        let user = self.user(token).await?;
        let material = self.material(token, id).await?;
        if material["uploader_id"] != user {
            return Err(Failure(403, "Only the uploader can finish this upload"));
        }
        Ok((user, material))
    }
    async fn admin(&self, token: &str) -> Result<()> {
        let user = self.user(token).await?;
        let (status, value) = self
            .json(
                reqwest::Method::GET,
                "/rest/v1/profiles",
                token,
                &[
                    ("id", format!("eq.{user}")),
                    ("select", "is_admin".into()),
                    ("limit", "1".into()),
                ],
                None,
            )
            .await?;
        if status != 200
            || value
                .as_array()
                .and_then(|rows| rows.first())
                .is_none_or(|row| row["is_admin"] != true)
        {
            return Err(Failure(403, "Admin only"));
        }
        Ok(())
    }
    async fn finish(&self, token: &str, id: Uuid, owner: &str, size: u64) -> Result<()> {
        self.storage.promote_async(id, size).await?;
        let (status, value) = self
            .json(
                reqwest::Method::PATCH,
                "/rest/v1/shared_materials",
                token,
                &[
                    ("id", format!("eq.{id}")),
                    ("uploader_id", format!("eq.{owner}")),
                    ("status", "eq.pending".into()),
                ],
                Some(json!({"status":"ready"})),
            )
            .await
            .map_err(|_| {
                Failure(
                    503,
                    "Could not finish upload; retry the upload status request",
                )
            })?;
        if status != 200 || value.as_array().is_none_or(|rows| rows.is_empty()) {
            return Err(Failure(
                503,
                "Could not finish upload; retry the upload status request",
            ));
        }
        self.storage.acknowledge(id)
    }
    fn writing(&self) -> Result<()> {
        if self.writes {
            Ok(())
        } else {
            Err(Failure(
                503,
                "Writes disabled until the legacy service is drained",
            ))
        }
    }
}
fn bearer(headers: &HeaderMap) -> Result<String> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(Failure(401, "Login required"))
}
fn size(material: &Value) -> Result<u64> {
    material["size_bytes"]
        .as_u64()
        .ok_or(Failure(413, "File is too large or has an invalid size"))
}
fn output(value: Value) -> Response {
    axum::Json(value).into_response()
}
fn declared(headers: &HeaderMap) -> Result<Option<u64>> {
    headers
        .get("content-length")
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| value.parse().ok())
                .ok_or(Failure(400, "Invalid file size"))
        })
        .transpose()
}
fn filename(value: &str) -> Result<HeaderValue> {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    let text = if encoded == value {
        format!("attachment; filename=\"{value}\"")
    } else {
        format!("attachment; filename*=utf-8''{encoded}")
    };
    HeaderValue::from_str(&text).map_err(|_| Failure(503, "Invalid download filename"))
}
struct ChunkWrite {
    file: File,
    start: u64,
    committed: bool,
}
impl Drop for ChunkWrite {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.file.set_len(self.start);
        }
    }
}
struct WholeWrite {
    storage: Storage,
    id: Uuid,
}
impl Drop for WholeWrite {
    fn drop(&mut self) {
        self.storage.abandon_whole(self.id);
    }
}
async fn handle(State(app): State<Arc<App>>, request: Request) -> Result<Response> {
    let path = request.uri().path();
    let method = request.method().as_str().to_owned();
    if path == "/health" && method == "GET" {
        app.storage.writable_probe()?;
        app.storage.object_probe().await?;
        let (status, _) = app
            .json(
                reqwest::Method::GET,
                "/rest/v1/shared_materials",
                &app.anon,
                &[("select", "id".into()), ("limit", "0".into())],
                None,
            )
            .await?;
        if status != 200 {
            return Err(Failure(503, "Materials database is unavailable"));
        }
        return Ok(output(json!({"ok":true})));
    }
    let token = bearer(request.headers())?;
    if (path == "/materials/quota" || path == "/admin/materials/usage") && method == "GET" {
        let admin = path.starts_with("/admin/");
        if admin {
            app.admin(&token).await?;
        } else {
            app.user(&token).await?;
        }
        let storage = app.storage.clone();
        let clean = app.writes;
        let usage = storage.usage_async(clean).await?;
        return Ok(output(if admin {
            let mut value =
                serde_json::to_value(usage).map_err(|_| Failure(503, "Cannot encode usage"))?;
            value["disk_total_bytes"] = json!(
                fs2::total_space(&app.storage.root)
                    .map_err(|_| Failure(503, "Storage unavailable"))?
            );
            value["disk_free_bytes"] = json!(
                fs2::free_space(&app.storage.root)
                    .map_err(|_| Failure(503, "Storage unavailable"))?
            );
            value
        } else {
            json!({"used_bytes":usage.used_bytes,"quota_bytes":usage.quota_bytes,"remaining_bytes":usage.remaining_bytes})
        }));
    }
    let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
    if parts.len() < 2 || parts[0] != "materials" {
        return Err(Failure(404, "Not Found"));
    }
    let id = Uuid::parse_str(parts[1]).map_err(|_| Failure(422, "Invalid material identifier"))?;
    if parts.len() == 3 && parts[2] == "content" && (method == "GET" || method == "HEAD") {
        app.user(&token).await?;
        let material = app.material(&token, id).await?;
        let paths = app.storage.paths(id);
        if material["status"] != "ready"
            || app.storage.completed_size(id).await? != Some(size(&material)?)
        {
            return Err(Failure(404, "File not found"));
        }
        let name = material["file_name"]
            .as_str()
            .ok_or(Failure(503, "Invalid download metadata"))?;
        let disposition = filename(name)?;
        let mut response = if app.storage.remote() {
            app.storage
                .object_response(id, size(&material)?, request.headers(), method == "HEAD")
                .await?
        } else {
            tower_http::services::ServeFile::new(&paths.complete)
                .oneshot(request)
                .await
                .map_err(|_| Failure(503, "File service is unavailable"))?
                .map(Body::new)
        };
        response
            .headers_mut()
            .insert("content-disposition", disposition);
        response.headers_mut().insert(
            "content-type",
            HeaderValue::from_static("application/octet-stream"),
        );
        return Ok(response);
    }
    app.writing()?;
    let _material_guard = app.storage.lock(id).lock_owned().await;
    let (owner, material) = app.owned(&token, id).await?;
    let expected = size(&material)?;
    let paths = app.storage.paths(id);
    if parts.len() == 2 && method == "DELETE" {
        let (status, value) = app
            .json(
                reqwest::Method::DELETE,
                "/rest/v1/shared_materials",
                &token,
                &[
                    ("id", format!("eq.{id}")),
                    ("uploader_id", format!("eq.{owner}")),
                ],
                None,
            )
            .await?;
        if status != 200 || value.as_array().is_none_or(|rows| rows.is_empty()) {
            return Err(Failure(503, "Could not delete material"));
        }
        app.storage.delete_async(id).await?;
        return Ok(output(json!({"ok":true})));
    }
    if parts.len() != 3 {
        return Err(Failure(404, "Not Found"));
    }
    if parts[2] == "upload" && method == "GET" {
        if material["status"] == "ready" {
            if app.storage.completed_size(id).await? != Some(expected) {
                return Err(Failure(404, "File not found"));
            }
            app.storage.acknowledge(id)?;
            return Ok(output(
                json!({"offset":expected,"size_bytes":expected,"complete":true,"chunk_bytes":MAX_CHUNK}),
            ));
        }
        if material["status"] != "pending" {
            return Err(Failure(403, "Upload is not pending"));
        }
        if app.storage.completed_size(id).await?.is_some()
            || app.storage.size(&paths.partial)? == Some(expected)
        {
            app.finish(&token, id, &owner, expected).await?;
            return Ok(output(
                json!({"offset":expected,"size_bytes":expected,"complete":true,"chunk_bytes":MAX_CHUNK}),
            ));
        }
        let offset = app.storage.size(&paths.partial)?.unwrap_or(0);
        if offset > expected {
            return Err(Failure(409, "Partial file is too large; delete and retry"));
        }
        return Ok(output(
            json!({"offset":offset,"size_bytes":expected,"complete":false,"chunk_bytes":MAX_CHUNK}),
        ));
    }
    if method != "PUT" || !["content", "chunks"].contains(&parts[2]) {
        return Err(Failure(405, "Method Not Allowed"));
    }
    let whole = parts[2] == "content";
    if material["status"] != "pending" {
        return Err(Failure(
            403,
            if whole {
                "Only the uploader can finish this upload"
            } else {
                "Upload is not pending"
            },
        ));
    }
    let mut query = reqwest::Url::parse("http://materials/").expect("fixed URI");
    query.set_query(request.uri().query());
    let offset = query
        .query_pairs()
        .filter(|(key, _)| key == "offset")
        .map(|(_, value)| value.into_owned())
        .last()
        .unwrap_or_else(|| "0".into())
        .parse::<u64>()
        .map_err(|_| Failure(400, "Invalid upload offset"))?;
    if offset > MAX_FILE {
        return Err(Failure(400, "Invalid upload offset"));
    }
    if whole
        && (app.storage.completed_size(id).await?.is_some()
            || app.storage.size(&paths.partial)?.is_some())
    {
        return Err(Failure(409, "File already exists or upload is in progress"));
    }
    if !whole && app.storage.completed_size(id).await?.is_some() {
        return Err(Failure(409, "File already exists; check upload status"));
    }
    if !whole && app.storage.size(&paths.partial)?.unwrap_or(0) != offset {
        return Err(Failure(
            409,
            "Upload offset does not match; check upload status",
        ));
    }
    let length = declared(request.headers())?;
    if let Some(length) = length {
        if whole && length != expected {
            return Err(Failure(400, "File size does not match the upload"));
        }
        if !whole && (length == 0 || length > MAX_CHUNK || offset.saturating_add(length) > expected)
        {
            return Err(Failure(
                413,
                "Chunk is too large or does not match the upload",
            ));
        }
    }
    app.storage.reserve_async(id, &owner, expected).await?;
    let _whole = whole.then(|| WholeWrite {
        storage: app.storage.clone(),
        id,
    });
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if whole || app.storage.size(&paths.partial)?.is_none() {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&paths.partial)
        .map_err(|_| Failure(503, "Cannot open upload storage"))?;
    let mut write = ChunkWrite {
        file,
        start: if whole { 0 } else { offset },
        committed: false,
    };
    write
        .file
        .seek(SeekFrom::Start(write.start))
        .map_err(|_| Failure(503, "Cannot seek upload storage"))?;
    let mut total = 0_u64;
    let mut stream = request.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Failure(400, "Upload was interrupted"))?;
        total = total.saturating_add(chunk.len() as u64);
        if total > if whole { MAX_FILE } else { MAX_CHUNK }
            || write.start.saturating_add(total) > expected
        {
            return Err(Failure(
                413,
                if whole {
                    "File size does not match the upload"
                } else {
                    "Chunk is too large or does not match the upload"
                },
            ));
        }
        write
            .file
            .write_all(&chunk)
            .map_err(|_| Failure(503, "Cannot write upload storage"))?;
    }
    if total == 0 || length.is_some_and(|length| total != length) || whole && total != expected {
        return Err(Failure(
            400,
            if whole {
                "File size does not match the upload"
            } else {
                "Chunk size does not match the request"
            },
        ));
    }
    write
        .file
        .sync_all()
        .map_err(|_| Failure(503, "Cannot persist upload storage"))?;
    write.committed = true;
    let complete = write.start + total == expected;
    if complete {
        app.finish(&token, id, &owner, expected).await?;
    }
    Ok(output(if whole {
        json!({"ok":true})
    } else {
        json!({"ok":true,"offset":write.start+total,"complete":complete})
    }))
}
pub fn router(app: App) -> Router {
    Router::new().fallback(handle).with_state(Arc::new(app))
}
