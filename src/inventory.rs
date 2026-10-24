//! Descriptor-relative, bounded, read-only approved input inventory.
use crate::{
    config::{ApprovedManifest, InputEntry},
    error::AppError,
    hash::sha256_reader,
    paths::ValidatedAppPath,
};
use rustix::fs::{self, Mode, OFlags};
use serde::Serialize;
use std::{
    ffi::CStr,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::fd::{AsFd, OwnedFd},
    path::{Component, Path},
};

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
fn invalid(message: impl Into<String>) -> AppError {
    AppError::validation(message)
}
fn open_relative(root: &OwnedFd, path: &str) -> Result<OwnedFd, AppError> {
    open_relative_optional(root, path)?
        .ok_or_else(|| invalid(format!("missing approved input: {path}")))
}
fn open_relative_optional(root: &OwnedFd, path: &str) -> Result<Option<OwnedFd>, AppError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(invalid("unsafe inventory path"));
    }
    let mut current =
        rustix::io::dup(root).map_err(|e| invalid(format!("duplicate app descriptor: {e}")))?;
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        let flags = if segments.peek().is_some() {
            DIR_FLAGS
        } else {
            FILE_FLAGS
        };
        current = match fs::openat(&current, segment, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(e) => return Err(invalid(format!("open approved input {path}: {e}"))),
        };
    }
    Ok(Some(current))
}
fn identity(fd: impl AsFd) -> Result<(u64, u64, u64, i64, i64), AppError> {
    let stat = fs::fstat(fd).map_err(|e| invalid(format!("stat input: {e}")))?;
    if !fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(invalid("approved input is not a regular file"));
    }
    Ok((
        stat.st_dev as u64,
        stat.st_ino as u64,
        stat.st_size as u64,
        stat.st_mtime,
        stat.st_mtime_nsec,
    ))
}
fn validate_scratch_outside_app(root: &OwnedFd, scratch: &Path) -> Result<(), AppError> {
    let absolute = if scratch.is_absolute() {
        scratch.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| invalid(format!("current directory: {e}")))?
            .join(scratch)
    };
    let app = fs::fstat(root).map_err(|e| invalid(format!("stat app root: {e}")))?;
    let mut current = fs::openat(fs::CWD, "/", DIR_FLAGS, Mode::empty())
        .map_err(|e| invalid(format!("open filesystem root: {e}")))?;
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid("unsafe scratch path"));
            }
            Component::Normal(name) => {
                current = fs::openat(&current, name, DIR_FLAGS, Mode::empty())
                    .map_err(|e| invalid(format!("open scratch component: {e}")))?;
                let stat =
                    fs::fstat(&current).map_err(|e| invalid(format!("stat scratch: {e}")))?;
                if (stat.st_dev, stat.st_ino) == (app.st_dev, app.st_ino) {
                    return Err(invalid("snapshot scratch is inside the app"));
                }
            }
        }
    }
    Ok(())
}
fn requested_parent_matches(output: &crate::paths::ValidatedOutputPath) -> Result<(), AppError> {
    let mut current = fs::openat(fs::CWD, "/", DIR_FLAGS, Mode::empty())
        .map_err(|e| invalid(format!("open output filesystem root: {e}")))?;
    for component in output.requested_parent().components() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid("unsafe output parent"));
            }
            Component::Normal(name) => {
                current = fs::openat(&current, name, DIR_FLAGS, Mode::empty())
                    .map_err(|e| invalid(format!("reopen requested output parent: {e}")))?;
            }
        }
    }
    let current_stat =
        fs::fstat(&current).map_err(|e| invalid(format!("stat requested parent: {e}")))?;
    let bound_stat =
        fs::fstat(output.parent()).map_err(|e| invalid(format!("stat bound parent: {e}")))?;
    if (current_stat.st_dev, current_stat.st_ino) != (bound_stat.st_dev, bound_stat.st_ino) {
        return Err(invalid("requested output parent changed"));
    }
    Ok(())
}

/// An approved file held through the exact descriptor that was hashed.
#[derive(Debug)]
pub struct VerifiedInputFile {
    file: File,
    root: OwnedFd,
    pub relative_path: String,
    pub size: u64,
    pub sha256: String,
    identity: (u64, u64, u64, i64, i64),
}
impl VerifiedInputFile {
    pub fn open(app: &ValidatedAppPath, expected: &InputEntry) -> Result<Self, AppError> {
        let root = rustix::io::dup(app.dir())
            .map_err(|e| invalid(format!("duplicate app descriptor: {e}")))?;
        let fd = open_relative(&root, &expected.relative_path)?;
        let before = identity(&fd)?;
        if before.2 != expected.size {
            return Err(invalid(format!("size drift: {}", expected.relative_path)));
        }
        let mut file = File::from(fd);
        let (size, sha256) = sha256_reader(&mut file)?;
        let after = identity(&file)?;
        if before != after || size != expected.size || sha256 != expected.sha256 {
            return Err(invalid(format!("input drift: {}", expected.relative_path)));
        }
        if identity(open_relative(&root, &expected.relative_path)?)? != after {
            return Err(invalid("approved path changed while hashing"));
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|e| invalid(format!("rewind input: {e}")))?;
        Ok(Self {
            file,
            root,
            relative_path: expected.relative_path.clone(),
            size,
            sha256,
            identity: after,
        })
    }
    pub fn recheck(&mut self) -> Result<(), AppError> {
        if identity(&self.file)? != self.identity || self.current_path_identity()? != self.identity
        {
            return Err(invalid("verified file identity drift"));
        }
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|e| invalid(format!("rewind input: {e}")))?;
        let (size, digest) = sha256_reader(&mut self.file)?;
        if size != self.size
            || digest != self.sha256
            || identity(&self.file)? != self.identity
            || self.current_path_identity()? != self.identity
        {
            return Err(invalid("verified file content drift"));
        }
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|e| invalid(format!("rewind input: {e}")))?;
        Ok(())
    }
    fn current_path_identity(&self) -> Result<(u64, u64, u64, i64, i64), AppError> {
        identity(open_relative(&self.root, &self.relative_path)?)
    }
    pub fn try_clone_file(&self) -> Result<File, AppError> {
        self.file
            .try_clone()
            .map_err(|e| invalid(format!("clone verified descriptor: {e}")))
    }
}
impl Read for VerifiedInputFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InventoryEntry {
    pub relative_path: String,
    pub size: u64,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InventorySnapshot {
    pub entries: Vec<InventoryEntry>,
}
impl InventorySnapshot {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, AppError> {
        serde_json::to_vec(self).map_err(|e| invalid(format!("serialize inventory: {e}")))
    }
}

/// Inspect named entries using only the app descriptor and no-follow opens.
pub fn inventory_entries(
    app: &ValidatedAppPath,
    entries: &[InputEntry],
) -> Result<InventorySnapshot, AppError> {
    let mut got = Vec::with_capacity(entries.len());
    for expected in entries {
        let file = VerifiedInputFile::open(app, expected)?;
        got.push(InventoryEntry {
            relative_path: file.relative_path,
            size: file.size,
            sha256: file.sha256,
        });
    }
    got.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    if got
        .windows(2)
        .any(|p| p[0].relative_path == p[1].relative_path)
    {
        return Err(invalid("duplicate inventory path"));
    }
    Ok(InventorySnapshot { entries: got })
}

fn listed_bundles(app: &ValidatedAppPath) -> Result<Vec<String>, AppError> {
    let folder = "Contents/Resources/Data/StreamingAssets/aa/StandaloneOSX";
    let mut current = rustix::io::dup(app.dir())
        .map_err(|e| invalid(format!("duplicate app descriptor: {e}")))?;
    for segment in folder.split('/') {
        current = fs::openat(&current, segment, DIR_FLAGS, Mode::empty())
            .map_err(|e| invalid(format!("open bundle directory: {e}")))?;
    }
    let mut dir =
        fs::Dir::read_from(&current).map_err(|e| invalid(format!("list bundle directory: {e}")))?;
    let mut result = Vec::new();
    while let Some(item) = dir
        .read()
        .transpose()
        .map_err(|e| invalid(format!("read bundle directory: {e}")))?
    {
        let name: &CStr = item.file_name();
        let text = name.to_str().map_err(|_| invalid("non-UTF8 bundle name"))?;
        if text.ends_with(".bundle")
            && (text.starts_with("general-voice-") || text.starts_with("naninovel-scripts_"))
        {
            result.push(format!("{folder}/{text}"));
        }
    }
    result.sort();
    Ok(result)
}
/// Full approved inventory. Added bundle leaves are validation failures.
pub fn inventory_app(
    app: &ValidatedAppPath,
    manifest: &ApprovedManifest,
) -> Result<InventorySnapshot, AppError> {
    let approved: Vec<_> = manifest
        .bundles
        .iter()
        .map(|b| b.relative_path.as_str())
        .collect();
    let listed = listed_bundles(app)?;
    if listed.iter().map(String::as_str).collect::<Vec<_>>() != approved {
        return Err(invalid("added or missing bundle leaf"));
    }
    inventory_entries(app, &manifest.entries())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateInspection {
    pub inventory: InventorySnapshot,
    pub added: Vec<String>,
    pub missing: Vec<String>,
    pub changed: Vec<String>,
}

fn probe_file(app: &ValidatedAppPath, path: &str) -> Result<Option<InventoryEntry>, AppError> {
    let Some(fd) = open_relative_optional(app.dir(), path)? else {
        return Ok(None);
    };
    let before = identity(&fd)?;
    let mut file = File::from(fd);
    let (size, sha256) = sha256_reader(&mut file)?;
    if identity(&file)? != before
        || size != before.2
        || identity(open_relative(app.dir(), path)?)? != before
    {
        return Err(invalid(format!("input changed while hashing: {path}")));
    }
    Ok(Some(InventoryEntry {
        relative_path: path.to_owned(),
        size,
        sha256,
    }))
}

/// Inspect the bounded consumed surface and classify drift without editing approval files.
pub fn inspect_entries(
    app: &ValidatedAppPath,
    approved: &[InputEntry],
) -> Result<CandidateInspection, AppError> {
    use std::collections::{BTreeMap, BTreeSet};
    let listed: BTreeSet<String> = listed_bundles(app)?.into_iter().collect();
    let expected: BTreeMap<&str, &InputEntry> = approved
        .iter()
        .map(|e| (e.relative_path.as_str(), e))
        .collect();
    if expected.len() != approved.len() {
        return Err(invalid("duplicate approved input path"));
    }
    let mut names: BTreeSet<String> = listed;
    names.extend(approved.iter().map(|e| e.relative_path.clone()));
    let mut result = CandidateInspection {
        inventory: InventorySnapshot {
            entries: Vec::new(),
        },
        added: Vec::new(),
        missing: Vec::new(),
        changed: Vec::new(),
    };
    for name in names {
        match probe_file(app, &name) {
            Ok(Some(actual)) => {
                match expected.get(name.as_str()) {
                    Some(want) if want.size != actual.size || want.sha256 != actual.sha256 => {
                        result.changed.push(name.clone())
                    }
                    None => result.added.push(name.clone()),
                    _ => {}
                }
                result.inventory.entries.push(actual);
            }
            Ok(None) if expected.contains_key(name.as_str()) => result.missing.push(name),
            Ok(None) => return Err(invalid("listed bundle disappeared during inspection")),
            Err(error) => return Err(error),
        }
    }
    Ok(result)
}

/// A bundle and its declared resources for one parser session/pass.
#[derive(Debug)]
pub struct VerifiedBundleInputs {
    pub bundle: VerifiedInputFile,
    pub resources: Vec<VerifiedInputFile>,
}
/// One bundle session's exact peak snapshot working set.
#[derive(Debug)]
pub struct VerifiedSnapshotSet {
    pub bundle: VerifiedSnapshot,
    pub resources: Vec<VerifiedSnapshot>,
    pub reserved_bytes: u64,
}
impl VerifiedBundleInputs {
    pub fn snapshot_set(&mut self, scratch: &Path) -> Result<VerifiedSnapshotSet, AppError> {
        validate_scratch_outside_app(&self.bundle.root, scratch)?;
        let reserved_bytes = self
            .resources
            .iter()
            .try_fold(self.bundle.size, |sum, resource| {
                sum.checked_add(resource.size)
            })
            .ok_or_else(|| invalid("snapshot working set overflow"))?;
        let free =
            fs::statvfs(scratch).map_err(|e| invalid(format!("check snapshot disk space: {e}")))?;
        let available = (free.f_bavail as u64)
            .checked_mul(free.f_frsize as u64)
            .ok_or_else(|| invalid("disk capacity overflow"))?;
        if available < reserved_bytes {
            return Err(invalid("insufficient bundle snapshot disk space"));
        }
        let bundle_size = self.bundle.size;
        let bundle = VerifiedSnapshot::copy_once(&mut self.bundle, scratch, bundle_size)?;
        let mut resources = Vec::with_capacity(self.resources.len());
        for resource in &mut self.resources {
            let resource_size = resource.size;
            resources.push(VerifiedSnapshot::copy_once(
                resource,
                scratch,
                resource_size,
            )?);
        }
        Ok(VerifiedSnapshotSet {
            bundle,
            resources,
            reserved_bytes,
        })
    }
}
pub trait DependencyDiscovery {
    fn discover(&self, bundle: &VerifiedInputFile) -> Result<Vec<String>, AppError>;
}
#[derive(Debug, Default)]
pub struct NoDependencies;
impl DependencyDiscovery for NoDependencies {
    fn discover(&self, _bundle: &VerifiedInputFile) -> Result<Vec<String>, AppError> {
        Ok(Vec::new())
    }
}
pub fn open_bundle_inputs(
    app: &ValidatedAppPath,
    manifest: &ApprovedManifest,
    bundle_path: &str,
    discovery: &impl DependencyDiscovery,
) -> Result<VerifiedBundleInputs, AppError> {
    let bundle = manifest
        .bundles
        .iter()
        .find(|b| b.relative_path == bundle_path)
        .ok_or_else(|| invalid("unapproved bundle"))?;
    let bundle = VerifiedInputFile::open(
        app,
        &InputEntry {
            relative_path: bundle.relative_path.clone(),
            size: bundle.size,
            sha256: bundle.sha256.clone(),
        },
    )?;
    let found = discovery.discover(&bundle)?;
    let mut resources = Vec::new();
    for name in found {
        let expected = manifest
            .resources
            .iter()
            .find(|r| r.relative_path == name)
            .ok_or_else(|| invalid("undeclared dependency resource"))?;
        resources.push(VerifiedInputFile::open(app, expected)?);
    }
    if resources.len() != manifest.resources.len() {
        return Err(invalid("declared dependency resource not discovered"));
    }
    Ok(VerifiedBundleInputs { bundle, resources })
}

/// A run-owned, content-addressed, read-only copy for path-only parser dependencies.
/// Dropping the value removes the entire owned session directory.
#[derive(Debug)]
pub struct VerifiedSnapshot {
    owned: tempfile::TempDir,
    path: std::path::PathBuf,
    pub size: u64,
    pub sha256: String,
}
impl VerifiedSnapshot {
    pub fn copy_once(
        input: &mut VerifiedInputFile,
        scratch: &std::path::Path,
        reservation_bytes: u64,
    ) -> Result<Self, AppError> {
        validate_scratch_outside_app(&input.root, scratch)?;
        if reservation_bytes != input.size {
            return Err(invalid("snapshot reservation does not match working set"));
        }
        let free =
            fs::statvfs(scratch).map_err(|e| invalid(format!("check snapshot disk space: {e}")))?;
        let available = (free.f_bavail as u64)
            .checked_mul(free.f_frsize as u64)
            .ok_or_else(|| invalid("disk capacity overflow"))?;
        if available < reservation_bytes {
            return Err(invalid("insufficient snapshot disk space"));
        }
        input.recheck()?;
        let owned = tempfile::Builder::new()
            .prefix("mgwt-snapshot-")
            .tempdir_in(scratch)
            .map_err(|e| invalid(format!("create snapshot directory: {e}")))?;
        let path = owned.path().join(&input.sha256);
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| invalid(format!("create snapshot: {e}")))?;
        input
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|e| invalid(format!("rewind verified input: {e}")))?;
        let copied = std::io::copy(&mut input.file, &mut target)
            .map_err(|e| invalid(format!("copy snapshot: {e}")))?;
        target
            .sync_all()
            .map_err(|e| invalid(format!("sync snapshot: {e}")))?;
        if copied != input.size {
            return Err(invalid("snapshot copy size drift"));
        }
        input.recheck()?;
        drop(target);
        let copy = File::open(&path).map_err(|e| invalid(format!("read snapshot: {e}")))?;
        let (size, digest) = sha256_reader(copy)?;
        if size != input.size || digest != input.sha256 {
            return Err(invalid("snapshot content drift"));
        }
        let mut permissions = std::fs::metadata(&path)
            .map_err(|e| invalid(format!("stat snapshot: {e}")))?
            .permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions)
            .map_err(|e| invalid(format!("protect snapshot: {e}")))?;
        Ok(Self {
            owned,
            path,
            size,
            sha256: digest,
        })
    }
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
    pub fn owned_directory(&self) -> &std::path::Path {
        self.owned.path()
    }
}

/// Atomically publish deterministic diagnostics and a separately named candidate inventory.
/// The reviewed configuration is never opened for writing.
pub fn write_candidate_reports(
    output: &crate::paths::ValidatedOutputPath,
    snapshot: &InventorySnapshot,
) -> Result<(), AppError> {
    let candidate = snapshot.canonical_bytes()?;
    let total_bytes: u64 = snapshot
        .entries
        .iter()
        .try_fold(0_u64, |acc, item| acc.checked_add(item.size))
        .ok_or_else(|| invalid("inventory size overflow"))?;
    let diagnostic=serde_json::to_vec(&serde_json::json!({"schema_version":"mgwt-inspect-diagnostic-v1","status":"match","entry_count":snapshot.entries.len(),"total_bytes":total_bytes,"candidate_sha256":crate::hash::sha256_bytes(&candidate),"added":[],"missing":[],"changed":[]})).map_err(|e|invalid(format!("serialize diagnostic: {e}")))?;
    publish_reports(output, &candidate, &diagnostic)
}

pub fn write_inspection_reports(
    output: &crate::paths::ValidatedOutputPath,
    inspection: &CandidateInspection,
) -> Result<(), AppError> {
    let candidate = inspection.inventory.canonical_bytes()?;
    let total_bytes: u64 = inspection
        .inventory
        .entries
        .iter()
        .try_fold(0_u64, |acc, item| acc.checked_add(item.size))
        .ok_or_else(|| invalid("inventory size overflow"))?;
    let status = if inspection.added.is_empty()
        && inspection.missing.is_empty()
        && inspection.changed.is_empty()
    {
        "match"
    } else {
        "drift"
    };
    let diagnostic=serde_json::to_vec(&serde_json::json!({"schema_version":"mgwt-inspect-diagnostic-v1","status":status,"entry_count":inspection.inventory.entries.len(),"total_bytes":total_bytes,"candidate_sha256":crate::hash::sha256_bytes(&candidate),"added":inspection.added,"missing":inspection.missing,"changed":inspection.changed})).map_err(|e|invalid(format!("serialize diagnostic: {e}")))?;
    publish_reports(output, &candidate, &diagnostic)
}

fn publish_reports(
    output: &crate::paths::ValidatedOutputPath,
    candidate: &[u8],
    diagnostic: &[u8],
) -> Result<(), AppError> {
    requested_parent_matches(output)?;
    use rustix::fs::{AtFlags, RenameFlags};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stage = format!(
        ".mgwt-candidate-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    fs::mkdirat(output.parent(), stage.as_str(), Mode::from_raw_mode(0o700))
        .map_err(|e| invalid(format!("create candidate stage: {e}")))?;
    let stage_fd = match fs::openat(output.parent(), stage.as_str(), DIR_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = fs::unlinkat(output.parent(), stage.as_str(), AtFlags::REMOVEDIR);
            return Err(invalid(format!("open candidate stage: {e}")));
        }
    };
    let result = (|| -> Result<(), AppError> {
        for (name, bytes) in [
            ("candidate-inventory.json", candidate),
            ("diagnostic.json", diagnostic),
        ] {
            let fd = fs::openat(
                &stage_fd,
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|e| invalid(format!("create candidate report: {e}")))?;
            let mut file = File::from(fd);
            file.write_all(bytes)
                .map_err(|e| invalid(format!("write candidate report: {e}")))?;
            file.sync_all()
                .map_err(|e| invalid(format!("sync candidate report: {e}")))?;
        }
        requested_parent_matches(output)?;
        fs::renameat_with(
            output.parent(),
            stage.as_str(),
            output.parent(),
            output.leaf(),
            RenameFlags::NOREPLACE,
        )
        .map_err(|e| invalid(format!("publish candidate reports: {e}")))?;
        Ok(())
    })();
    if result.is_err() {
        for name in ["candidate-inventory.json", "diagnostic.json"] {
            let _ = fs::unlinkat(&stage_fd, name, AtFlags::empty());
        }
        let _ = fs::unlinkat(output.parent(), stage.as_str(), AtFlags::REMOVEDIR);
    }
    result
}
