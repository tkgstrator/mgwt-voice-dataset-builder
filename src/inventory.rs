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
    sync::Arc,
};
use unity_rs_core::source::{Region, Source};

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
    #[cfg(target_os = "linux")]
    let mtime_nsec = i64::try_from(stat.st_mtime_nsec)
        .map_err(|_| invalid("input modification timestamp is out of range"))?;
    #[cfg(not(target_os = "linux"))]
    let mtime_nsec = stat.st_mtime_nsec;
    Ok((
        stat.st_dev as u64,
        stat.st_ino as u64,
        stat.st_size as u64,
        stat.st_mtime,
        mtime_nsec,
    ))
}
fn validate_scratch_outside_app(root: &OwnedFd, scratch: &Path) -> Result<OwnedFd, AppError> {
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
    Ok(current)
}
pub(crate) fn requested_parent_matches(
    output: &crate::paths::ValidatedOutputPath,
) -> Result<(), AppError> {
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
    /// Duplicate the already-verified descriptors without reopening an app path.
    pub fn duplicate_bound(&self) -> Result<Self, AppError> {
        Ok(Self {
            file: File::from(
                rustix::io::dup(&self.file)
                    .map_err(|e| invalid(format!("duplicate verified input: {e}")))?,
            ),
            root: rustix::io::dup(&self.root)
                .map_err(|e| invalid(format!("duplicate app root: {e}")))?,
            relative_path: self.relative_path.clone(),
            size: self.size,
            sha256: self.sha256.clone(),
            identity: self.identity,
        })
    }
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
    bundle: VerifiedSnapshot,
    resources: Vec<VerifiedSnapshot>,
    reserved_bytes: u64,
}
impl VerifiedSnapshotSet {
    /// Pass verified byte regions to a parser without reopening snapshot paths.
    pub(crate) fn with_parser_regions<T>(
        &self,
        open: impl FnOnce(&Region, &[(String, Region)]) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        self.verify_all()?;
        let bundle = self.bundle.region()?;
        let resources = self
            .resources
            .iter()
            .map(|resource| Ok((resource.relative_path.clone(), resource.region()?)))
            .collect::<Result<Vec<_>, AppError>>()?;
        let result = open(&bundle, &resources);
        self.verify_all()?;
        result
    }

    /// Use only descriptor-backed, owned snapshot paths while retaining this set.
    pub fn with_parser_paths<T>(
        &self,
        open: impl FnOnce(&Path, &[(String, std::path::PathBuf)]) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        self.verify_all()?;
        let paths: Vec<_> = self
            .resources
            .iter()
            .map(|resource| (resource.relative_path.clone(), resource.descriptor_path()))
            .collect();
        let result = open(&self.bundle.descriptor_path(), &paths);
        self.verify_all()?;
        result
    }
    pub fn checkpoint(&self) -> Result<(), AppError> {
        self.verify_all()
    }
    pub fn bundle(&self) -> &VerifiedSnapshot {
        &self.bundle
    }
    pub fn resources(&self) -> &[VerifiedSnapshot] {
        &self.resources
    }
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }
    fn verify_all(&self) -> Result<(), AppError> {
        self.bundle.verify_owned_bytes()?;
        for resource in &self.resources {
            resource.verify_owned_bytes()?;
        }
        Ok(())
    }
    /// Open a lazy bundle parser while retaining its bundle and resource snapshots.
    pub fn start_parser_session<P>(
        self,
        open: impl FnOnce(&Path, &[(String, std::path::PathBuf)]) -> Result<P, AppError>,
    ) -> Result<VerifiedBundleParserSession<P>, AppError> {
        self.verify_all()?;
        let bundle_path = self.bundle.descriptor_path();
        let resource_paths: Vec<_> = self
            .resources
            .iter()
            .map(|resource| (resource.relative_path.clone(), resource.descriptor_path()))
            .collect();
        let parser = open(&bundle_path, &resource_paths)?;
        self.verify_all()?;
        Ok(VerifiedBundleParserSession {
            parser,
            snapshots: self,
        })
    }
}
/// A lazy parser with its entire verified bundle/resource working set held alive.
#[derive(Debug)]
pub struct VerifiedBundleParserSession<P> {
    parser: P,
    snapshots: VerifiedSnapshotSet,
}
impl<P> VerifiedBundleParserSession<P> {
    pub fn run(self, consume: impl FnOnce(&mut P) -> Result<(), AppError>) -> Result<(), AppError> {
        let Self {
            mut parser,
            snapshots,
        } = self;
        let result = consume(&mut parser);
        drop(parser);
        snapshots.verify_all()?;
        result
    }
}
impl VerifiedBundleInputs {
    /// Produce one owned snapshot set from already-bound descriptors through a shared input.
    pub fn snapshot_set_shared(&self, scratch: &Path) -> Result<VerifiedSnapshotSet, AppError> {
        let mut copy = Self {
            bundle: self.bundle.duplicate_bound()?,
            resources: self
                .resources
                .iter()
                .map(VerifiedInputFile::duplicate_bound)
                .collect::<Result<Vec<_>, _>>()?,
        };
        copy.snapshot_set(scratch)
    }
    pub fn snapshot_set(&mut self, scratch: &Path) -> Result<VerifiedSnapshotSet, AppError> {
        let bundle_root =
            fs::fstat(&self.bundle.root).map_err(|e| invalid(format!("stat bundle root: {e}")))?;
        for resource in &self.resources {
            let resource_root = fs::fstat(&resource.root)
                .map_err(|e| invalid(format!("stat resource root: {e}")))?;
            if (bundle_root.st_dev, bundle_root.st_ino)
                != (resource_root.st_dev, resource_root.st_ino)
            {
                return Err(invalid("bundle and resource app roots differ"));
            }
        }
        let scratch_fd = validate_scratch_outside_app(&self.bundle.root, scratch)?;
        let reserved_bytes = self
            .resources
            .iter()
            .try_fold(self.bundle.size, |sum, resource| {
                sum.checked_add(resource.size)
            })
            .ok_or_else(|| invalid("snapshot working set overflow"))?;
        let free = fs::fstatvfs(&scratch_fd)
            .map_err(|e| invalid(format!("check snapshot disk space: {e}")))?;
        let available = (free.f_bavail as u64)
            .checked_mul(free.f_frsize as u64)
            .ok_or_else(|| invalid("disk capacity overflow"))?;
        if available < reserved_bytes {
            return Err(invalid("insufficient bundle snapshot disk space"));
        }
        let bundle_size = self.bundle.size;
        let bundle = VerifiedSnapshot::copy_once_on_bound(
            &mut self.bundle,
            rustix::io::dup(&scratch_fd)
                .map_err(|e| invalid(format!("duplicate scratch descriptor: {e}")))?,
            bundle_size,
            &mut |_| Ok(()),
        )?;
        let mut resources = Vec::with_capacity(self.resources.len());
        for resource in &mut self.resources {
            let resource_size = resource.size;
            resources.push(VerifiedSnapshot::copy_once_on_bound(
                resource,
                rustix::io::dup(&scratch_fd)
                    .map_err(|e| invalid(format!("duplicate scratch descriptor: {e}")))?,
                resource_size,
                &mut |_| Ok(()),
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
    fn discover(&self, bundle: &VerifiedSnapshot) -> Result<Vec<String>, AppError>;
}
#[derive(Debug, Default)]
pub struct NoDependencies;
impl DependencyDiscovery for NoDependencies {
    fn discover(&self, _bundle: &VerifiedSnapshot) -> Result<Vec<String>, AppError> {
        Ok(Vec::new())
    }
}
pub fn open_bundle_inputs(
    app: &ValidatedAppPath,
    manifest: &ApprovedManifest,
    bundle_path: &str,
    scratch: &Path,
    discovery: &impl DependencyDiscovery,
) -> Result<VerifiedSnapshotSet, AppError> {
    let bundle = manifest
        .bundles
        .iter()
        .find(|b| b.relative_path == bundle_path)
        .ok_or_else(|| invalid("unapproved bundle"))?;
    let mut bundle = VerifiedInputFile::open(
        app,
        &InputEntry {
            relative_path: bundle.relative_path.clone(),
            size: bundle.size,
            sha256: bundle.sha256.clone(),
        },
    )?;
    let scratch_fd = validate_scratch_outside_app(&bundle.root, scratch)?;
    let bundle_size = bundle.size;
    let bundle_snapshot = VerifiedSnapshot::copy_once_on_bound(
        &mut bundle,
        rustix::io::dup(&scratch_fd)
            .map_err(|e| invalid(format!("duplicate scratch descriptor: {e}")))?,
        bundle_size,
        &mut |_| Ok(()),
    )?;
    let found = discovery.discover(&bundle_snapshot)?;
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
    let remaining_bytes = resources
        .iter()
        .try_fold(0_u64, |sum, resource| sum.checked_add(resource.size))
        .ok_or_else(|| invalid("resource snapshot working set overflow"))?;
    let reserved_bytes = bundle_size
        .checked_add(remaining_bytes)
        .ok_or_else(|| invalid("bundle snapshot working set overflow"))?;
    let free = fs::fstatvfs(&scratch_fd)
        .map_err(|e| invalid(format!("check resource snapshot disk space: {e}")))?;
    let available = (free.f_bavail as u64)
        .checked_mul(free.f_frsize as u64)
        .ok_or_else(|| invalid("disk capacity overflow"))?;
    if available < remaining_bytes {
        return Err(invalid("insufficient resource snapshot disk space"));
    }
    let mut resource_snapshots = Vec::with_capacity(resources.len());
    for mut resource in resources {
        let size = resource.size;
        resource_snapshots.push(VerifiedSnapshot::copy_once_on_bound(
            &mut resource,
            rustix::io::dup(&scratch_fd)
                .map_err(|e| invalid(format!("duplicate scratch descriptor: {e}")))?,
            size,
            &mut |_| Ok(()),
        )?);
    }
    Ok(VerifiedSnapshotSet {
        bundle: bundle_snapshot,
        resources: resource_snapshots,
        reserved_bytes,
    })
}

/// Callbacks at deterministic snapshot boundaries support cancellation and race checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotPhase {
    ScratchBound,
    StageCreated,
    Copied(u64),
}

/// A private directory created and removed relative to the bound scratch descriptor.
#[derive(Debug)]
struct OwnedSnapshotDir {
    scratch: OwnedFd,
    stage: OwnedFd,
    name: String,
    files: Vec<String>,
}
impl OwnedSnapshotDir {
    fn new(scratch: OwnedFd) -> Result<Self, AppError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            ".mgwt-snapshot-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        fs::mkdirat(&scratch, name.as_str(), Mode::from_raw_mode(0o700))
            .map_err(|e| invalid(format!("create bound snapshot directory: {e}")))?;
        let stage = match fs::openat(&scratch, name.as_str(), DIR_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                let _ = fs::unlinkat(&scratch, name.as_str(), fs::AtFlags::REMOVEDIR);
                return Err(invalid(format!("open bound snapshot directory: {error}")));
            }
        };
        Ok(Self {
            scratch,
            stage,
            name,
            files: Vec::new(),
        })
    }
}
impl Drop for OwnedSnapshotDir {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = fs::unlinkat(&self.stage, file.as_str(), fs::AtFlags::empty());
        }
        let _ = fs::unlinkat(&self.scratch, self.name.as_str(), fs::AtFlags::REMOVEDIR);
    }
}

/// A run-owned, content-addressed, read-only copy for parser dependencies.
/// The source descriptor is never exposed to a parser.
#[derive(Debug)]
pub struct VerifiedSnapshot {
    owned: OwnedSnapshotDir,
    file: File,
    relative_path: String,
    size: u64,
    sha256: String,
}
/// A lazy parser kept alive together with the verified snapshot descriptor.
#[derive(Debug)]
pub struct VerifiedParserSession<P> {
    parser: P,
    snapshot: VerifiedSnapshot,
}
impl<P> VerifiedParserSession<P> {
    /// Consume the parser while its snapshot remains held, then verify after consumption.
    pub fn run(self, consume: impl FnOnce(&mut P) -> Result<(), AppError>) -> Result<(), AppError> {
        let Self {
            mut parser,
            snapshot,
        } = self;
        let result = consume(&mut parser);
        drop(parser);
        snapshot.verify_owned_bytes()?;
        result
    }
}

struct SnapshotFileSource {
    file: File,
    length: u64,
}

impl Source for SnapshotFileSource {
    fn len(&self) -> u64 {
        self.length
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(output, offset)
        }
        #[cfg(windows)]
        {
            use std::{io, os::windows::fs::FileExt};
            let mut filled = 0;
            while filled < output.len() {
                let at = offset.checked_add(filled as u64).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "snapshot offset overflow")
                })?;
                match self.file.seek_read(&mut output[filled..], at) {
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(read) => filled += read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (offset, output);
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }
}

impl VerifiedSnapshot {
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn copy_once(
        input: &mut VerifiedInputFile,
        scratch: &Path,
        reservation_bytes: u64,
    ) -> Result<Self, AppError> {
        Self::copy_once_with_checkpoint(input, scratch, reservation_bytes, |_| Ok(()))
    }

    /// Copy exactly the reserved bytes. A checkpoint error aborts and cleans owned files.
    pub fn copy_once_with_checkpoint(
        input: &mut VerifiedInputFile,
        scratch: &Path,
        reservation_bytes: u64,
        mut checkpoint: impl FnMut(SnapshotPhase) -> Result<(), AppError>,
    ) -> Result<Self, AppError> {
        let scratch_fd = validate_scratch_outside_app(&input.root, scratch)?;
        Self::copy_once_on_bound(input, scratch_fd, reservation_bytes, &mut checkpoint)
    }

    fn copy_once_on_bound(
        input: &mut VerifiedInputFile,
        scratch_fd: OwnedFd,
        reservation_bytes: u64,
        checkpoint: &mut impl FnMut(SnapshotPhase) -> Result<(), AppError>,
    ) -> Result<Self, AppError> {
        use std::io::Write;
        if reservation_bytes != input.size {
            return Err(invalid("snapshot reservation does not match working set"));
        }
        let free = fs::fstatvfs(&scratch_fd)
            .map_err(|e| invalid(format!("check snapshot disk space: {e}")))?;
        let available = (free.f_bavail as u64)
            .checked_mul(free.f_frsize as u64)
            .ok_or_else(|| invalid("disk capacity overflow"))?;
        if available < reservation_bytes {
            return Err(invalid("insufficient snapshot disk space"));
        }
        input.recheck()?;
        checkpoint(SnapshotPhase::ScratchBound)?;
        let mut owned = OwnedSnapshotDir::new(scratch_fd)?;
        checkpoint(SnapshotPhase::StageCreated)?;
        let fd = fs::openat(
            &owned.stage,
            input.sha256.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| invalid(format!("create bound snapshot file: {e}")))?;
        owned.files.push(input.sha256.clone());
        let mut target = File::from(fd);
        input
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|e| invalid(format!("rewind verified input: {e}")))?;
        let mut copied = 0_u64;
        let mut chunk = [0_u8; 64 * 1024];
        while copied < input.size {
            let remaining = (input.size - copied).min(chunk.len() as u64) as usize;
            let count = input
                .file
                .read(&mut chunk[..remaining])
                .map_err(|e| invalid(format!("read snapshot source: {e}")))?;
            if count == 0 {
                return Err(invalid("snapshot source shrank during copy"));
            }
            target
                .write_all(&chunk[..count])
                .map_err(|e| invalid(format!("write snapshot: {e}")))?;
            copied += count as u64;
            checkpoint(SnapshotPhase::Copied(copied))?;
        }
        let mut extra = [0_u8; 1];
        if input
            .file
            .read(&mut extra)
            .map_err(|e| invalid(format!("check source growth: {e}")))?
            != 0
        {
            return Err(invalid("snapshot source grew beyond reservation"));
        }
        target
            .sync_all()
            .map_err(|e| invalid(format!("sync snapshot: {e}")))?;
        input.recheck()?;
        let fd = fs::openat(
            &owned.stage,
            input.sha256.as_str(),
            FILE_FLAGS,
            Mode::empty(),
        )
        .map_err(|e| invalid(format!("open bound snapshot for verification: {e}")))?;
        let mut file = File::from(fd);
        let (size, digest) = sha256_reader(&mut file)?;
        if size != input.size || digest != input.sha256 {
            return Err(invalid("snapshot content drift"));
        }
        fs::fchmod(&target, Mode::from_raw_mode(0o400))
            .map_err(|e| invalid(format!("protect snapshot: {e}")))?;
        drop(target);
        file.seek(SeekFrom::Start(0))
            .map_err(|e| invalid(format!("rewind snapshot: {e}")))?;
        Ok(Self {
            owned,
            file,
            relative_path: input.relative_path.clone(),
            size,
            sha256: digest,
        })
    }

    fn verify_owned_bytes(&self) -> Result<(), AppError> {
        let fd = fs::openat(
            &self.owned.stage,
            self.sha256.as_str(),
            FILE_FLAGS,
            Mode::empty(),
        )
        .map_err(|e| invalid(format!("open snapshot for handoff: {e}")))?;
        let stat = identity(&fd)?;
        if stat != identity(&self.file)? {
            return Err(invalid("snapshot path identity drift"));
        }
        let (size, digest) = sha256_reader(File::from(fd))?;
        if size != self.size || digest != self.sha256 {
            return Err(invalid("snapshot bytes changed"));
        }
        Ok(())
    }

    fn region(&self) -> Result<Region, AppError> {
        let file = self
            .file
            .try_clone()
            .map_err(|error| invalid(format!("clone verified snapshot handle: {error}")))?;
        Region::new(
            Arc::new(SnapshotFileSource {
                file,
                length: self.size,
            }),
            0,
            self.size,
        )
        .map_err(|error| invalid(format!("bind snapshot region: {error}")))
    }

    /// Read the verified copy through a bounded-lifetime reader for dependency discovery.
    pub fn with_reader<T>(
        &self,
        inspect: impl FnOnce(&mut dyn Read) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        self.verify_owned_bytes()?;
        let fd = fs::openat(
            &self.owned.stage,
            self.sha256.as_str(),
            FILE_FLAGS,
            Mode::empty(),
        )
        .map_err(|e| invalid(format!("open snapshot reader: {e}")))?;
        let mut file = File::from(fd);
        let result = inspect(&mut file);
        self.verify_owned_bytes()?;
        result
    }

    /// Open a path-only lazy parser and retain this snapshot until its consumption finishes.
    pub fn start_parser_session<P>(
        self,
        open: impl FnOnce(&Path) -> Result<P, AppError>,
    ) -> Result<VerifiedParserSession<P>, AppError> {
        self.verify_owned_bytes()?;
        let descriptor_path = self.descriptor_path();
        let parser = open(&descriptor_path)?;
        self.verify_owned_bytes()?;
        Ok(VerifiedParserSession {
            parser,
            snapshot: self,
        })
    }
    fn descriptor_path(&self) -> std::path::PathBuf {
        use std::os::fd::AsRawFd;
        std::path::PathBuf::from(format!("/dev/fd/{}", self.file.as_raw_fd()))
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
