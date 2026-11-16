use crate::{Failure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, FileTimes, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime},
};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;
pub const MAX_FILE: u64 = 1024 * 1024 * 1024;
pub const MAX_CHUNK: u64 = 8 * 1024 * 1024;
const PARTIAL_TTL: Duration = Duration::from_secs(86400);
#[derive(Clone)]
pub struct Storage {
    pub root: PathBuf,
    pub quota: u64,
    serial: Arc<Mutex<()>>,
    locks: Arc<Mutex<HashMap<Uuid, Weak<AsyncMutex<()>>>>>,
    _instance: Arc<File>,
}
#[derive(Serialize, Debug, Default)]
pub struct Usage {
    pub used_bytes: u64,
    pub reserved_bytes: u64,
    pub quota_bytes: u64,
    pub remaining_bytes: u64,
    pub file_count: u64,
}
#[derive(Deserialize, Serialize)]
struct Reservation {
    uploader_id: String,
    size_bytes: u64,
}
#[derive(Clone, Debug)]
pub struct Paths {
    pub complete: PathBuf,
    pub partial: PathBuf,
    pub reservation: PathBuf,
}
fn unavailable() -> Failure {
    Failure(503, "Materials storage is unavailable")
}
fn ordinary(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}
fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(unavailable()),
    }
}
fn read_manifest(path: &Path) -> Result<Reservation> {
    if !ordinary(path) {
        return Err(Failure(
            409,
            "Upload reservation is invalid; delete and retry",
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| unavailable())?
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable())?;
    if bytes.len() > 65536 {
        return Err(Failure(
            409,
            "Upload reservation is invalid; delete and retry",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Failure(409, "Upload reservation is invalid; delete and retry"))
}
impl Storage {
    pub fn open(root: PathBuf, quota: u64) -> Result<Self> {
        if !root.is_dir() {
            return Err(unavailable());
        }
        let root = root.canonicalize().map_err(|_| unavailable())?;
        let instance = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(".cbgpu-materials.lock"))
            .map_err(|_| unavailable())?;
        fs2::FileExt::try_lock_exclusive(&instance).map_err(|_| {
            Failure(
                503,
                "Another Rust materials writer already owns this storage",
            )
        })?;
        Ok(Self {
            root,
            quota,
            serial: Arc::new(Mutex::new(())),
            locks: Arc::new(Mutex::new(HashMap::new())),
            _instance: Arc::new(instance),
        })
    }
    pub fn lock(&self, id: Uuid) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lock) = locks.get(&id).and_then(Weak::upgrade) {
            return lock;
        }
        if locks.len() > 4096 {
            locks.retain(|_, value| value.strong_count() > 0);
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(id, Arc::downgrade(&lock));
        lock
    }
    pub fn paths(&self, id: Uuid) -> Paths {
        let name = id.simple().to_string();
        let complete = self.root.join(&name[..2]).join(&name);
        Paths {
            partial: complete.with_extension("partial"),
            reservation: complete.with_extension("upload.json"),
            complete,
        }
    }
    pub fn size(&self, path: &Path) -> Result<Option<u64>> {
        match fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_file() => Ok(Some(m.len())),
            Ok(_) => Err(unavailable()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(unavailable()),
        }
    }
    fn shard(&self, paths: &Paths) -> Result<()> {
        let parent = paths.complete.parent().ok_or_else(unavailable)?;
        fs::create_dir_all(parent).map_err(|_| unavailable())?;
        if !fs::symlink_metadata(parent)
            .map_err(|_| unavailable())?
            .file_type()
            .is_dir()
        {
            return Err(unavailable());
        }
        Ok(())
    }
    fn cleanup(&self) {
        let cutoff = SystemTime::now()
            .checked_sub(PARTIAL_TTL)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let Ok(shards) = fs::read_dir(&self.root) else {
            return;
        };
        for shard in shards.flatten() {
            if !shard.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(entries) = fs::read_dir(shard.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                if !entry.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if !(name.ends_with(".partial") || name.ends_with(".upload.json")) {
                    continue;
                }
                let stem = name.split('.').next().unwrap_or("");
                let Ok(id) = Uuid::parse_str(stem) else {
                    continue;
                };
                let paths = self.paths(id);
                if paths.complete.parent() != Some(shard.path().as_path())
                    || paths.complete.file_name().and_then(|s| s.to_str()) != Some(stem)
                {
                    continue;
                }
                let lock = self.lock(id);
                let Ok(_material) = lock.try_lock() else {
                    continue;
                };
                let existing = [&paths.partial, &paths.reservation]
                    .into_iter()
                    .filter(|p| ordinary(p))
                    .collect::<Vec<_>>();
                if !existing.is_empty()
                    && existing.iter().all(|p| {
                        fs::metadata(p)
                            .and_then(|m| m.modified())
                            .is_ok_and(|modified| modified < cutoff)
                    })
                {
                    for path in existing {
                        let _ = remove(path);
                    }
                }
            }
        }
    }
    fn scan(&self) -> Result<Usage> {
        let mut used = 0_u64;
        let mut reserved = 0_u64;
        let mut count = 0_u64;
        for shard in fs::read_dir(&self.root)
            .map_err(|_| unavailable())?
            .flatten()
        {
            if !shard.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let mut partials = HashMap::<String, u64>::new();
            let mut manifests = HashMap::<String, u64>::new();
            let mut completed = HashSet::new();
            for entry in fs::read_dir(shard.path())
                .map_err(|_| unavailable())?
                .flatten()
            {
                if !entry.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(stem) = name.strip_suffix(".partial") {
                    if let Ok(metadata) = entry.metadata() {
                        partials.insert(stem.to_owned(), metadata.len());
                    }
                } else if let Some(stem) = name.strip_suffix(".upload.json") {
                    if let Ok(manifest) = read_manifest(&entry.path())
                        && manifest.size_bytes > 0
                        && manifest.size_bytes <= MAX_FILE
                    {
                        manifests.insert(stem.to_owned(), manifest.size_bytes);
                    }
                } else if Uuid::parse_str(&name).is_ok()
                    && let Ok(metadata) = entry.metadata()
                {
                    used = used.saturating_add(metadata.len());
                    count += 1;
                    completed.insert(name);
                }
            }
            for key in partials
                .keys()
                .chain(manifests.keys())
                .collect::<HashSet<_>>()
            {
                if !completed.contains(key) {
                    reserved = reserved.saturating_add(
                        partials
                            .get(key)
                            .copied()
                            .unwrap_or(0)
                            .max(manifests.get(key).copied().unwrap_or(0)),
                    );
                }
            }
        }
        Ok(Usage {
            used_bytes: used,
            reserved_bytes: reserved,
            quota_bytes: self.quota,
            remaining_bytes: self.quota.saturating_sub(used.saturating_add(reserved)),
            file_count: count,
        })
    }
    pub fn usage(&self, clean: bool) -> Result<Usage> {
        let _serial = self.serial.lock().map_err(|_| unavailable())?;
        if clean {
            self.cleanup();
        }
        self.scan()
    }
    pub fn reserve(&self, id: Uuid, owner: &str, size: u64) -> Result<Paths> {
        if !(1..=MAX_FILE).contains(&size) {
            return Err(Failure(413, "File is too large or has an invalid size"));
        }
        let paths = self.paths(id);
        self.shard(&paths)?;
        let _serial = self.serial.lock().map_err(|_| unavailable())?;
        if paths.reservation.exists() {
            let saved = read_manifest(&paths.reservation)?;
            if saved.uploader_id != owner || saved.size_bytes != size {
                return Err(Failure(409, "Upload metadata changed; delete and retry"));
            }
        } else {
            self.cleanup();
            let usage = self.scan()?;
            let existing = self.size(&paths.partial)?.unwrap_or(0);
            if size.saturating_sub(existing) > usage.remaining_bytes {
                return Err(Failure(507, "Materials storage is full"));
            }
            let mut temporary =
                tempfile::NamedTempFile::new_in(paths.complete.parent().ok_or_else(unavailable)?)
                    .map_err(|_| unavailable())?;
            serde_json::to_writer(
                &mut temporary,
                &Reservation {
                    uploader_id: owner.to_owned(),
                    size_bytes: size,
                },
            )
            .map_err(|_| unavailable())?;
            temporary.as_file().sync_all().map_err(|_| unavailable())?;
            temporary
                .persist(&paths.reservation)
                .map_err(|_| unavailable())?;
        }
        File::open(&paths.reservation)
            .and_then(|file| file.set_times(FileTimes::new().set_modified(SystemTime::now())))
            .map_err(|_| unavailable())?;
        Ok(paths)
    }
    pub fn promote(&self, id: Uuid, size: u64) -> Result<()> {
        let _serial = self.serial.lock().map_err(|_| unavailable())?;
        let paths = self.paths(id);
        if self.size(&paths.complete)?.is_none() {
            if self.size(&paths.partial)? != Some(size) {
                return Err(Failure(409, "Upload is incomplete"));
            }
            fs::rename(&paths.partial, &paths.complete).map_err(|_| unavailable())?;
        }
        if self.size(&paths.complete)? != Some(size) {
            return Err(Failure(409, "File size does not match the upload"));
        }
        Ok(())
    }
    pub fn acknowledge(&self, id: Uuid) -> Result<()> {
        let _serial = self.serial.lock().map_err(|_| unavailable())?;
        remove(&self.paths(id).reservation)
    }
    pub fn delete(&self, id: Uuid) -> Result<()> {
        let _serial = self.serial.lock().map_err(|_| unavailable())?;
        let p = self.paths(id);
        remove(&p.complete)?;
        remove(&p.partial)?;
        remove(&p.reservation)
    }
    pub fn abandon_whole(&self, id: Uuid) {
        let Ok(_serial) = self.serial.lock() else {
            return;
        };
        let p = self.paths(id);
        if !p.complete.exists() {
            let _ = remove(&p.partial);
            let _ = remove(&p.reservation);
        }
    }
    pub fn writable_probe(&self) -> Result<()> {
        if fs2::available_space(&self.root).map_err(|_| unavailable())? == 0 {
            return Err(unavailable());
        }
        let mut file = tempfile::tempfile_in(&self.root).map_err(|_| unavailable())?;
        file.write_all(b"cbgpu-ready").map_err(|_| unavailable())?;
        file.flush().map_err(|_| unavailable())
    }
}
