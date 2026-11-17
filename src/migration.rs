//! Explicit offline storage-copy tooling. Never changes metadata or deletes source objects.
use crate::{Failure, Result, objects::Objects};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;
const MAX_MANIFEST: u64 = 16 * 1024 * 1024;
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub id: Uuid,
    pub size: u64,
    pub sha256: String,
}
#[derive(Debug)]
pub struct Manifest {
    pub entries: Vec<Entry>,
    pub sha256: String,
}
fn invalid() -> Failure {
    Failure(400, "Invalid or unsafe migration input")
}
impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() as u64 > MAX_MANIFEST || !bytes.ends_with(b"\n") {
            return Err(invalid());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
        if text.contains('\r') {
            return Err(invalid());
        }
        let mut entries = Vec::new();
        let mut previous = String::new();
        for line in text.lines() {
            let parts = line.split('\t').collect::<Vec<_>>();
            if parts.len() != 3 {
                return Err(invalid());
            }
            let id = Uuid::parse_str(parts[0]).map_err(|_| invalid())?;
            let size = parts[1].parse::<u64>().map_err(|_| invalid())?;
            if id.is_nil()
                || id.to_string() != parts[0]
                || parts[0] <= previous.as_str()
                || size == 0
                || size > crate::storage::MAX_FILE
                || size.to_string() != parts[1]
                || parts[2].len() != 64
                || !parts[2]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(invalid());
            }
            previous = parts[0].to_owned();
            entries.push(Entry {
                id,
                size,
                sha256: parts[2].to_owned(),
            });
            if entries.len() > 100_000 {
                return Err(invalid());
            }
        }
        Ok(Self {
            entries,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        })
    }
    pub fn read(path: &Path) -> Result<Self> {
        let mut file = open_no_follow(path)?;
        if file.metadata().map_err(|_| invalid())?.len() > MAX_MANIFEST {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(MAX_MANIFEST + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid())?;
        Self::parse(&bytes)
    }
}
/// Traverses each directory component through directory descriptors on Unix.
/// No symlink (including an ancestor) is followed. No source lockfile is created.
#[cfg(unix)]
pub fn open_no_follow(path: &Path) -> Result<File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    let absolute = absolute_clean(path)?;
    let mut directory = File::open("/").map_err(|_| invalid())?;
    let parts = absolute
        .components()
        .filter_map(|v| {
            if let std::path::Component::Normal(p) = v {
                Some(p)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return Err(invalid());
    }
    for (index, part) in parts.iter().enumerate() {
        let name = CString::new(part.as_bytes()).map_err(|_| invalid())?;
        let flags = libc::O_RDONLY
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if index + 1 < parts.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(invalid());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    if !directory.metadata().map_err(|_| invalid())?.is_file() {
        return Err(invalid());
    }
    Ok(directory)
}
#[cfg(not(unix))]
pub fn open_no_follow(path: &Path) -> Result<File> {
    let absolute = absolute_clean(path)?;
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component);
        if fs::symlink_metadata(&current)
            .map_err(|_| invalid())?
            .file_type()
            .is_symlink()
        {
            return Err(invalid());
        }
    }
    let file = File::open(&absolute).map_err(|_| invalid())?;
    if !file.metadata().map_err(|_| invalid())?.is_file() {
        return Err(invalid());
    }
    Ok(file)
}
fn absolute_clean(path: &Path) -> Result<PathBuf> {
    if path.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    }) {
        return Err(invalid());
    }
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_err(|_| invalid())?.join(path)
    })
}
pub fn checked_root(path: &Path) -> Result<PathBuf> {
    let absolute = absolute_clean(path)?;
    if fs::canonicalize(&absolute).map_err(|_| invalid())? != absolute
        || !fs::symlink_metadata(&absolute)
            .map_err(|_| invalid())?
            .is_dir()
    {
        return Err(invalid());
    }
    Ok(absolute)
}
fn digest_file(file: &mut File, entry: &Entry, mut output: Option<&mut File>) -> Result<()> {
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut bytes = [0u8; 65536];
    loop {
        let count = file.read(&mut bytes).map_err(|_| invalid())?;
        if count == 0 {
            break;
        }
        total = total.checked_add(count as u64).ok_or_else(invalid)?;
        if total > entry.size {
            return Err(Failure(409, "Source size differs from manifest"));
        }
        digest.update(&bytes[..count]);
        if let Some(target) = output.as_mut() {
            target.write_all(&bytes[..count]).map_err(|_| invalid())?;
        }
    }
    if total != entry.size || format!("{:x}", digest.finalize()) != entry.sha256 {
        return Err(Failure(409, "Source SHA-256 differs from manifest"));
    }
    Ok(())
}
fn local_path(root: &Path, id: Uuid) -> PathBuf {
    let name = id.simple().to_string();
    root.join(&name[..2]).join(name)
}
pub fn local_snapshot(root: &Path, entry: &Entry) -> Result<tempfile::NamedTempFile> {
    let mut input = open_no_follow(&local_path(root, entry.id))?;
    let mut snapshot = tempfile::NamedTempFile::new().map_err(|_| invalid())?;
    digest_file(&mut input, entry, Some(snapshot.as_file_mut()))?;
    snapshot.as_file().sync_all().map_err(|_| invalid())?;
    Ok(snapshot)
}
/// The report must be a NEW private file outside both storage roots. Never overwrites.
pub fn planned_destination(path: &Path) -> Result<PathBuf> {
    let absolute = absolute_clean(path)?;
    checked_root(absolute.parent().ok_or_else(invalid)?)?;
    Ok(absolute)
}
pub fn report_path(path: &Path, root: &Path) -> Result<PathBuf> {
    let absolute = absolute_clean(path)?;
    checked_root(absolute.parent().ok_or_else(invalid)?)?;
    if absolute.starts_with(root) || fs::symlink_metadata(&absolute).is_ok() {
        return Err(invalid());
    }
    Ok(absolute)
}
pub fn write_report(path: &Path, report: &impl Serialize, local_root: &Path) -> Result<()> {
    let absolute = absolute_clean(path)?;
    let parent = checked_root(absolute.parent().ok_or_else(invalid)?)?;
    if absolute.starts_with(local_root) || absolute.exists() {
        return Err(invalid());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&parent).map_err(|_| invalid())?;
    serde_json::to_writer_pretty(&mut temporary, report).map_err(|_| invalid())?;
    temporary.write_all(b"\n").map_err(|_| invalid())?;
    temporary.as_file().sync_all().map_err(|_| invalid())?;
    temporary
        .persist_noclobber(&absolute)
        .map_err(|_| invalid())?;
    File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| invalid())?;
    Ok(())
}
#[derive(Serialize)]
pub struct ItemReport {
    pub id: Uuid,
    pub size: u64,
    pub sha256: String,
    pub result: &'static str,
}
#[derive(Serialize)]
pub struct Report {
    pub version: u8,
    pub direction: String,
    pub manifest_sha256: String,
    pub local_root: PathBuf,
    pub object_location_sha256: Option<String>,
    pub apply: bool,
    pub writers_fence_acknowledged: bool,
    pub external_writers_inspected: bool,
    pub items: Vec<ItemReport>,
}
pub fn new_destination(path: &Path, manifest: &Manifest, resume: bool) -> Result<PathBuf> {
    let absolute = absolute_clean(path)?;
    checked_root(absolute.parent().ok_or_else(invalid)?)?;
    let receipt = serde_json::json!({"version":1,"manifest_sha256":manifest.sha256,"direction":"s3-to-local","root":absolute});
    if resume {
        let root = checked_root(&absolute)?;
        let mut file = open_no_follow(&root.join(".cbgpu-migration.json"))?;
        if file.metadata().map_err(|_| invalid())?.len() > 4096 {
            return Err(invalid());
        }
        let value: serde_json::Value = serde_json::from_reader(&mut file).map_err(|_| invalid())?;
        if value != receipt {
            return Err(Failure(
                409,
                "Destination receipt does not match this migration",
            ));
        }
        return Ok(root);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&absolute)
            .map_err(|_| Failure(409, "Reverse destination must be a NEW directory"))?;
    }
    #[cfg(not(unix))]
    fs::create_dir(&absolute)
        .map_err(|_| Failure(409, "Reverse destination must be a NEW directory"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(absolute.join(".cbgpu-migration.json"))
        .map_err(|_| invalid())?;
    serde_json::to_writer(&mut file, &receipt).map_err(|_| invalid())?;
    file.sync_all().map_err(|_| invalid())?;
    Ok(absolute)
}
pub async fn copy_inventory(
    objects: &Objects,
    manifest: &Manifest,
    root: &Path,
    reverse: bool,
    apply: bool,
) -> Result<Report> {
    let mut report = Report {
        version: 1,
        direction: if reverse {
            "s3-to-local"
        } else {
            "local-to-s3"
        }
        .into(),
        manifest_sha256: manifest.sha256.clone(),
        local_root: root.to_owned(),
        object_location_sha256: None,
        apply,
        writers_fence_acknowledged: apply,
        external_writers_inspected: false,
        items: Vec::new(),
    };
    for entry in &manifest.entries {
        let result = if reverse {
            if objects.size(entry.id).await? != Some(entry.size)
                || objects.sha256(entry.id).await? != entry.sha256
            {
                return Err(Failure(409, "Remote source differs from manifest"));
            }
            let path = local_path(root, entry.id);
            if path.try_exists().map_err(|_| invalid())? {
                let mut file = open_no_follow(&path)?;
                digest_file(&mut file, entry, None)?;
                "VERIFIED_EXISTING"
            } else if apply {
                let parent = path.parent().ok_or_else(invalid)?;
                if !parent.exists() {
                    fs::create_dir(parent).map_err(|_| invalid())?;
                }
                checked_root(parent)?;
                let mut temporary =
                    tempfile::NamedTempFile::new_in(parent).map_err(|_| invalid())?;
                objects
                    .download_verified(entry.id, temporary.as_file_mut(), entry.size, &entry.sha256)
                    .await?;
                temporary.as_file().sync_all().map_err(|_| invalid())?;
                temporary
                    .persist_noclobber(&path)
                    .map_err(|_| Failure(409, "Destination appeared during copy"))?;
                File::open(parent)
                    .and_then(|f| f.sync_all())
                    .map_err(|_| invalid())?;
                let mut readback = open_no_follow(&path)?;
                digest_file(&mut readback, entry, None)?;
                "COPIED_VERIFIED"
            } else {
                "SOURCE_VERIFIED"
            }
        } else {
            let snapshot = local_snapshot(root, entry)?;
            if let Some(size) = objects.size(entry.id).await? {
                if size != entry.size || objects.sha256(entry.id).await? != entry.sha256 {
                    return Err(Failure(
                        409,
                        "Existing destination differs; refusing overwrite",
                    ));
                }
                "VERIFIED_EXISTING"
            } else if apply {
                objects
                    .put_file(entry.id, snapshot.path(), entry.size)
                    .await?;
                if objects.size(entry.id).await? != Some(entry.size)
                    || objects.sha256(entry.id).await? != entry.sha256
                {
                    return Err(Failure(409, "Destination read-back differs from manifest"));
                }
                "COPIED_VERIFIED"
            } else {
                "SOURCE_VERIFIED"
            }
        };
        report.items.push(ItemReport {
            id: entry.id,
            size: entry.size,
            sha256: entry.sha256.clone(),
            result,
        });
    }
    Ok(report)
}
