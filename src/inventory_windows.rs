//! Handle-relative, bounded, read-only approved input inventory on Windows.
use crate::{
    config::{ApprovedManifest, InputEntry},
    error::AppError,
    hash::sha256_reader,
    paths::{ValidatedAppPath, component_is_safe, open_validated_directory},
};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, MetadataExt as _, OpenOptions, OpenOptionsExt as _};
use serde::Serialize;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::windows::fs::MetadataExt as _,
    path::Path,
    sync::Arc,
};
use unity_rs_core::source::{Region, Source};

const REPARSE_POINT: u32 = 0x400;
const FILE_SHARE_READ: u32 = 0x0000_0001;
fn invalid(message: impl Into<String>) -> AppError {
    AppError::validation(message)
}
fn open_relative(root: &Dir, path: &str) -> Result<File, AppError> {
    open_relative_optional(root, path)?
        .ok_or_else(|| invalid(format!("missing approved input: {path}")))
}
fn open_relative_optional(root: &Dir, path: &str) -> Result<Option<File>, AppError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path
            .split('/')
            .any(|p| p.is_empty() || !component_is_safe(std::ffi::OsStr::new(p)))
    {
        return Err(invalid("unsafe inventory path"));
    }
    let mut current = root
        .try_clone()
        .map_err(|e| invalid(format!("duplicate app directory: {e}")))?;
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_some() {
            current = match current.open_dir_nofollow(segment) {
                Ok(dir) => dir,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(invalid(format!("open approved directory {path}: {e}"))),
            };
            if current
                .dir_metadata()
                .map_err(|e| invalid(format!("stat approved directory: {e}")))?
                .file_attributes()
                & REPARSE_POINT
                != 0
            {
                return Err(invalid("reparse point in approved input path"));
            }
        } else {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .follow(FollowSymlinks::No)
                .share_mode(FILE_SHARE_READ);
            let file = match current.open_with(segment, &options) {
                Ok(file) => file.into_std(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(invalid(format!("open approved input {path}: {e}"))),
            };
            if file
                .metadata()
                .map_err(|e| invalid(format!("stat approved input: {e}")))?
                .file_attributes()
                & REPARSE_POINT
                != 0
            {
                return Err(invalid("approved input is a reparse point"));
            }
            return Ok(Some(file));
        }
    }
    Err(invalid("empty approved input path"))
}
type InputIdentity = (u64, [u8; 16], u64, u64, u64);

fn identity(file: &File) -> Result<InputIdentity, AppError> {
    let stat = file
        .metadata()
        .map_err(|e| invalid(format!("stat input: {e}")))?;
    if !stat.is_file() || stat.file_attributes() & REPARSE_POINT != 0 {
        return Err(invalid("approved input is not a regular file"));
    }
    let file_id = mgwt_windows_native::file_identity(file)
        .map_err(|e| invalid(format!("identify input: {e}")))?;
    Ok((
        file_id.volume_serial_number,
        file_id.file_id,
        stat.len(),
        stat.last_write_time(),
        stat.creation_time(),
    ))
}
fn validate_scratch_outside_app(root: &Dir, scratch: &Path) -> Result<Dir, AppError> {
    open_validated_directory(scratch, Some(root))
}
pub(crate) fn requested_parent_matches(
    output: &crate::paths::ValidatedOutputPath,
) -> Result<(), AppError> {
    let current = open_validated_directory(output.requested_parent(), None)?;
    if mgwt_windows_native::file_identity(&current)
        .map_err(|e| invalid(format!("identify requested parent: {e}")))?
        != mgwt_windows_native::file_identity(output.parent())
            .map_err(|e| invalid(format!("identify bound parent: {e}")))?
    {
        return Err(invalid("requested output parent changed"));
    }
    Ok(())
}

/// An approved file held through the exact descriptor that was hashed.
#[derive(Debug)]
pub struct VerifiedInputFile {
    file: File,
    root: Dir,
    pub relative_path: String,
    pub size: u64,
    pub sha256: String,
    identity: InputIdentity,
}
impl VerifiedInputFile {
    /// Duplicate the already-verified descriptors without reopening an app path.
    pub fn duplicate_bound(&self) -> Result<Self, AppError> {
        Ok(Self {
            file: self
                .file
                .try_clone()
                .map_err(|e| invalid(format!("duplicate verified input: {e}")))?,
            root: self
                .root
                .try_clone()
                .map_err(|e| invalid(format!("duplicate app root: {e}")))?,
            relative_path: self.relative_path.clone(),
            size: self.size,
            sha256: self.sha256.clone(),
            identity: self.identity,
        })
    }
    pub fn open(app: &ValidatedAppPath, expected: &InputEntry) -> Result<Self, AppError> {
        let root = app
            .dir()
            .try_clone()
            .map_err(|e| invalid(format!("duplicate app descriptor: {e}")))?;
        let mut file = open_relative(&root, &expected.relative_path)?;
        let before = identity(&file)?;
        if before.2 != expected.size {
            return Err(invalid(format!("size drift: {}", expected.relative_path)));
        }
        let (size, sha256) = sha256_reader(&mut file)?;
        let after = identity(&file)?;
        if before != after || size != expected.size || sha256 != expected.sha256 {
            return Err(invalid(format!("input drift: {}", expected.relative_path)));
        }
        if identity(&open_relative(&root, &expected.relative_path)?)? != after {
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
    fn current_path_identity(&self) -> Result<InputIdentity, AppError> {
        identity(&open_relative(&self.root, &self.relative_path)?)
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
    let mut current = app
        .dir()
        .try_clone()
        .map_err(|e| invalid(format!("duplicate app descriptor: {e}")))?;
    for segment in folder.split('/') {
        current = current
            .open_dir_nofollow(segment)
            .map_err(|e| invalid(format!("open bundle directory: {e}")))?;
        if current
            .dir_metadata()
            .map_err(|e| invalid(format!("stat bundle directory: {e}")))?
            .file_attributes()
            & REPARSE_POINT
            != 0
        {
            return Err(invalid("reparse point in bundle directory"));
        }
    }
    let mut result = Vec::new();
    let entries = current
        .read_dir(".")
        .map_err(|e| invalid(format!("list bundle directory: {e}")))?;
    for item in entries {
        let item = item.map_err(|e| invalid(format!("read bundle directory: {e}")))?;
        let name = item.file_name();
        let text = name
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 bundle name"))?;
        if text.ends_with(".bundle")
            && (text.starts_with("general-voice-") || text.starts_with("naninovel-scripts_"))
        {
            if !component_is_safe(std::ffi::OsStr::new(text)) {
                return Err(invalid("unsafe bundle filename"));
            }
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
    let Some(mut file) = open_relative_optional(app.dir(), path)? else {
        return Ok(None);
    };
    let before = identity(&file)?;
    let (size, sha256) = sha256_reader(&mut file)?;
    if identity(&file)? != before
        || size != before.2
        || identity(&open_relative(app.dir(), path)?)? != before
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
        let bundle_root = mgwt_windows_native::file_identity(&self.bundle.root)
            .map_err(|e| invalid(format!("identify bundle root: {e}")))?;
        for resource in &self.resources {
            let resource_root = mgwt_windows_native::file_identity(&resource.root)
                .map_err(|e| invalid(format!("identify resource root: {e}")))?;
            if bundle_root != resource_root {
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
        let available = mgwt_windows_native::available_space(&scratch_fd)
            .map_err(|e| invalid(format!("check snapshot disk space: {e}")))?;
        if available < reserved_bytes {
            return Err(invalid("insufficient bundle snapshot disk space"));
        }
        let bundle_size = self.bundle.size;
        let bundle = VerifiedSnapshot::copy_once_on_bound(
            &mut self.bundle,
            scratch_fd
                .try_clone()
                .map_err(|e| invalid(format!("duplicate scratch descriptor: {e}")))?,
            bundle_size,
            &mut |_| Ok(()),
        )?;
        let mut resources = Vec::with_capacity(self.resources.len());
        for resource in &mut self.resources {
            let resource_size = resource.size;
            resources.push(VerifiedSnapshot::copy_once_on_bound(
                resource,
                scratch_fd
                    .try_clone()
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
        scratch_fd
            .try_clone()
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
    let available = mgwt_windows_native::available_space(&scratch_fd)
        .map_err(|e| invalid(format!("check resource snapshot disk space: {e}")))?;
    if available < remaining_bytes {
        return Err(invalid("insufficient resource snapshot disk space"));
    }
    let mut resource_snapshots = Vec::with_capacity(resources.len());
    for mut resource in resources {
        let size = resource.size;
        resource_snapshots.push(VerifiedSnapshot::copy_once_on_bound(
            &mut resource,
            scratch_fd
                .try_clone()
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
    stage: Option<mgwt_windows_native::StageGuard>,
    name: String,
    files: Vec<String>,
}
impl OwnedSnapshotDir {
    fn stage(&self) -> &Dir {
        self.stage
            .as_ref()
            .expect("snapshot stage exists until cleanup")
            .dir()
    }

    fn new(scratch: Dir) -> Result<Self, AppError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            ".mgwt-snapshot-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let stage = mgwt_windows_native::create_pinned_stage(&scratch, std::ffi::OsStr::new(&name))
            .map_err(|e| invalid(format!("create bound snapshot directory: {e}")))?;
        Ok(Self {
            stage: Some(stage),
            name,
            files: Vec::new(),
        })
    }
}
impl Drop for OwnedSnapshotDir {
    fn drop(&mut self) {
        if let Some(stage) = self.stage.take() {
            for file in &self.files {
                if let Err(error) = stage.dir().remove_file(file) {
                    eprintln!("snapshot cleanup left {file}: {error}");
                }
            }
            if let Err(error) = stage.remove_empty() {
                eprintln!("snapshot cleanup left {}: {error}", self.name);
            }
        }
    }
}

/// A run-owned, content-addressed, read-only copy for parser dependencies.
/// The source descriptor is never exposed to a parser.
#[derive(Debug)]
pub struct VerifiedSnapshot {
    file: File,
    owned: OwnedSnapshotDir,
    relative_path: String,
    size: u64,
    sha256: String,
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
        scratch_fd: Dir,
        reservation_bytes: u64,
        checkpoint: &mut impl FnMut(SnapshotPhase) -> Result<(), AppError>,
    ) -> Result<Self, AppError> {
        use std::io::Write;
        if reservation_bytes != input.size {
            return Err(invalid("snapshot reservation does not match working set"));
        }
        let available = mgwt_windows_native::available_space(&scratch_fd)
            .map_err(|e| invalid(format!("check snapshot disk space: {e}")))?;
        if available < reservation_bytes {
            return Err(invalid("insufficient snapshot disk space"));
        }
        input.recheck()?;
        checkpoint(SnapshotPhase::ScratchBound)?;
        let mut owned = OwnedSnapshotDir::new(scratch_fd)?;
        checkpoint(SnapshotPhase::StageCreated)?;
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut target = owned
            .stage()
            .open_with(&input.sha256, &options)
            .map_err(|e| invalid(format!("create bound snapshot file: {e}")))?
            .into_std();
        owned.files.push(input.sha256.clone());
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
        drop(target);
        let mut file = open_relative(owned.stage(), &input.sha256)?;
        let (size, digest) = sha256_reader(&mut file)?;
        if size != input.size || digest != input.sha256 {
            return Err(invalid("snapshot content drift"));
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|e| invalid(format!("rewind snapshot: {e}")))?;
        Ok(Self {
            file,
            owned,
            relative_path: input.relative_path.clone(),
            size,
            sha256: digest,
        })
    }

    fn verify_owned_bytes(&self) -> Result<(), AppError> {
        let file = open_relative(self.owned.stage(), &self.sha256)?;
        let stat = identity(&file)?;
        if stat != identity(&self.file)? {
            return Err(invalid("snapshot path identity drift"));
        }
        let (size, digest) = sha256_reader(file)?;
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
        let mut file = open_relative(self.owned.stage(), &self.sha256)?;
        let result = inspect(&mut file);
        self.verify_owned_bytes()?;
        result
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
    let _parent_pins =
        mgwt_windows_native::pin_requested_parent_path(output.requested_parent(), output.parent())
            .map_err(|e| invalid(format!("pin report output ancestors: {e}")))?;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stage = format!(
        ".mgwt-candidate-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let stage_guard =
        mgwt_windows_native::create_pinned_stage(output.parent(), std::ffi::OsStr::new(&stage))
            .map_err(|e| invalid(format!("create candidate stage: {e}")))?;
    let stage_identity = stage_guard.identity();
    let result = (|| -> Result<(), AppError> {
        for (name, bytes) in [
            ("candidate-inventory.json", candidate),
            ("diagnostic.json", diagnostic),
        ] {
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            let mut file = stage_guard
                .dir()
                .open_with(name, &options)
                .map_err(|e| invalid(format!("create candidate report: {e}")))?
                .into_std();
            file.write_all(bytes)
                .map_err(|e| invalid(format!("write candidate report: {e}")))?;
            file.sync_all()
                .map_err(|e| invalid(format!("sync candidate report: {e}")))?;
        }
        requested_parent_matches(output)?;
        Ok(())
    })();
    if result.is_err() {
        for name in ["candidate-inventory.json", "diagnostic.json"] {
            let _ = stage_guard.dir().remove_file(name);
        }
        if let Err(error) = stage_guard.remove_empty() {
            eprintln!("candidate report cleanup left {stage}: {error}");
        }
        return result;
    }
    let result = stage_guard.promote(output.leaf());
    if let Err(error) = result {
        if let Err(cleanup_error) = cleanup_report_stage(output.parent(), &stage, stage_identity) {
            eprintln!("candidate report cleanup left {stage}: {cleanup_error}");
        }
        return Err(invalid(format!("publish candidate reports: {error}")));
    }
    Ok(())
}

fn cleanup_report_stage(
    parent: &Dir,
    stage: &str,
    identity: mgwt_windows_native::FileIdentity,
) -> Result<(), AppError> {
    let stage_dir = parent
        .open_dir_nofollow(stage)
        .map_err(|e| invalid(format!("open report stage for cleanup: {e}")))?;
    if mgwt_windows_native::file_identity(&stage_dir)
        .map_err(|e| invalid(format!("identify report stage for cleanup: {e}")))?
        != identity
    {
        return Err(invalid("report stage changed before cleanup"));
    }
    for name in ["candidate-inventory.json", "diagnostic.json"] {
        match stage_dir.remove_file(name) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(invalid(format!("remove report stage file: {error}"))),
        }
    }
    drop(stage_dir);
    mgwt_windows_native::remove_directory_if_identity(parent, std::ffi::OsStr::new(stage), identity)
        .map_err(|e| invalid(format!("remove report stage: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::ValidatedOutputPath;
    use sha2::{Digest, Sha256};
    use std::fs;

    fn approved(name: &str, bytes: &[u8]) -> InputEntry {
        InputEntry {
            relative_path: name.to_owned(),
            size: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(bytes)),
        }
    }

    #[test]
    fn snapshot_uses_verified_handle_and_cleans_owned_stage() {
        let root = tempfile::tempdir().unwrap();
        let app_path = root.path().join("manosaba.app");
        let scratch = root.path().join("scratch");
        fs::create_dir(&app_path).unwrap();
        fs::create_dir(&scratch).unwrap();
        fs::write(app_path.join("bundle"), b"approved bytes").unwrap();
        let app = ValidatedAppPath::new(&app_path).unwrap();
        let mut input =
            VerifiedInputFile::open(&app, &approved("bundle", b"approved bytes")).unwrap();
        let size = input.size;
        let snapshot = VerifiedSnapshot::copy_once(&mut input, &scratch, size).unwrap();
        let bytes = snapshot
            .with_reader(|reader| {
                let mut bytes = Vec::new();
                reader
                    .read_to_end(&mut bytes)
                    .map_err(|e| invalid(e.to_string()))?;
                Ok(bytes)
            })
            .unwrap();
        assert_eq!(bytes, b"approved bytes");
        drop(snapshot);
        assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
    }

    #[test]
    fn verified_input_handle_rejects_concurrent_writer_and_detects_later_drift() {
        let root = tempfile::tempdir().unwrap();
        let app_path = root.path().join("manosaba.app");
        fs::create_dir(&app_path).unwrap();
        let input_path = app_path.join("bundle");
        fs::write(&input_path, b"approved bytes").unwrap();
        let app = ValidatedAppPath::new(&app_path).unwrap();
        let expected = approved("bundle", b"approved bytes");
        let input = VerifiedInputFile::open(&app, &expected).unwrap();
        assert!(fs::write(&input_path, b"tampered").is_err());
        drop(input);
        fs::write(&input_path, b"tampered").unwrap();
        assert!(VerifiedInputFile::open(&app, &expected).is_err());
    }

    #[test]
    fn report_publication_does_not_replace_existing_output() {
        let root = tempfile::tempdir().unwrap();
        let app_path = root.path().join("manosaba.app");
        fs::create_dir(&app_path).unwrap();
        let app = ValidatedAppPath::new(&app_path).unwrap();
        let target = root.path().join("reports");
        let output = ValidatedOutputPath::new(&target, &app).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("marker"), b"keep").unwrap();
        let inventory = InventorySnapshot {
            entries: Vec::new(),
        };
        assert!(write_candidate_reports(&output, &inventory).is_err());
        assert_eq!(fs::read(target.join("marker")).unwrap(), b"keep");
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".mgwt-candidate-")
        }));
    }
}
