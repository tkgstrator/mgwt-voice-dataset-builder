#![cfg(windows)]

use cap_std::fs::Dir;
use std::{
    ffi::{OsStr, OsString, c_void},
    fs::File,
    io,
    mem::{MaybeUninit, offset_of, size_of},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr,
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_CREATE, FILE_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION,
            FILE_RENAME_INFORMATION_0, FILE_SYNCHRONOUS_IO_NONALERT, FileFsDeviceInformation,
            FileFsFullSizeInformation, FileRenameInformation, NtCreateFile,
            NtQueryVolumeInformationFile, NtSetInformationFile,
        },
        System::SystemServices::{
            FILE_FS_DEVICE_INFORMATION, FILE_FS_FULL_SIZE_INFORMATION, FILE_REMOTE_DEVICE,
        },
    },
    Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE, RtlNtStatusToDosError, UNICODE_STRING},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION,
            AclSizeInformation, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE,
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetKernelObjectSecurity, GetLengthSid, GetSecurityDescriptorControl,
            GetSecurityDescriptorDacl, GetTokenInformation, InitializeAcl,
            InitializeSecurityDescriptor, IsValidSid, OBJECT_INHERIT_ACE, SE_DACL_PROTECTED,
            SECURITY_DESCRIPTOR, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
            TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            CreateFileW, DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FileAttributeTagInfo,
            FileIdInfo, GetDriveTypeW, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
            GetVolumeInformationByHandleW, OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE,
        },
        System::{
            IO::IO_STATUS_BLOCK,
            Kernel::OBJ_CASE_INSENSITIVE,
            SystemServices::{FILE_PERSISTENT_ACLS, SECURITY_DESCRIPTOR_REVISION},
            Threading::{GetCurrentProcess, OpenProcessToken},
            WindowsProgramming::{DRIVE_FIXED, FILE_CREATED},
        },
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct FileIdentity {
    pub volume_serial_number: u64,
    pub file_id: [u8; 16],
}

pub fn file_identity(handle: &impl AsHandle) -> io::Result<FileIdentity> {
    let mut info = MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: the borrowed handle remains open for the call, and `info` has the
    // exact size and alignment required by FileIdInfo. Success initializes it.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            raw_handle(handle),
            FileIdInfo,
            info.as_mut_ptr().cast::<c_void>(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetFileInformationByHandleEx succeeded and filled FILE_ID_INFO.
    let info = unsafe { info.assume_init() };
    let file_id = info.FileId.Identifier;
    if file_id == [0; 16] {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "volume does not provide a stable 128-bit file ID",
        ));
    }
    Ok(FileIdentity {
        volume_serial_number: info.VolumeSerialNumber,
        file_id,
    })
}

pub fn rename_directory_noreplace(
    parent: &Dir,
    source_leaf: &OsStr,
    destination_leaf: &OsStr,
    expected_source: FileIdentity,
) -> io::Result<()> {
    let source_leaf = literal_leaf(source_leaf)?;
    let destination_leaf = literal_leaf(destination_leaf)?;
    require_local_fixed_volume(parent)?;
    let source = open_source_for_delete(parent, source_leaf)?;
    if file_identity(&source)? != expected_source {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source directory identity changed",
        ));
    }
    let target_parent = open_pinned_directory(&final_path(parent)?, true)?;
    if file_identity(&target_parent)? != file_identity(parent)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "target parent identity changed",
        ));
    }
    let name_bytes = destination_leaf
        .encode_wide()
        .count()
        .checked_mul(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "leaf name is too long"))?;
    // The NT information class requires a full struct plus the variable UTF-16
    // data, including its minimum fixed-size footprint for one-character names.
    let payload_bytes = size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(name_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer overflow"))?;
    let payload_len = u32::try_from(payload_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer overflow"))?;
    let mut buffer = vec![0_u64; payload_bytes.div_ceil(size_of::<u64>())];
    let rename = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let wide: Vec<u16> = destination_leaf.encode_wide().collect();
    let mut status_block = MaybeUninit::<IO_STATUS_BLOCK>::uninit();
    // SAFETY: `buffer` is 8-byte aligned and at least payload_bytes long;
    // offset_of! addresses the flexible UTF-16 array. Both handles remain open.
    // fs_at opened the source with FILE_SYNCHRONOUS_IO_NONALERT, so the NT call
    // completes before these buffers are dropped.
    unsafe {
        (*rename).Anonymous = FILE_RENAME_INFORMATION_0 { ReplaceIfExists: 0 };
        (*rename).RootDirectory = raw_handle(&target_parent);
        (*rename).FileNameLength = name_bytes as u32;
        ptr::copy_nonoverlapping(
            wide.as_ptr(),
            rename
                .cast::<u8>()
                .add(offset_of!(FILE_RENAME_INFORMATION, FileName))
                .cast::<u16>(),
            wide.len(),
        );
        let status = NtSetInformationFile(
            raw_handle(&source),
            status_block.as_mut_ptr(),
            rename.cast::<c_void>(),
            payload_len,
            FileRenameInformation,
        );
        if status != 0 {
            let code = RtlNtStatusToDosError(status);
            return Err(io::Error::from_raw_os_error(code as i32));
        }
    }
    Ok(())
}

pub fn remove_directory_if_identity(
    parent: &Dir,
    leaf: &OsStr,
    expected: FileIdentity,
) -> io::Result<()> {
    use fs_at::os::windows::FileExt;
    let source = open_source_for_delete(parent, literal_leaf(leaf)?)?;
    if file_identity(&source)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source directory identity changed",
        ));
    }
    source.delete_by_handle().map_err(|(_, error)| error)
}

pub fn available_space(parent: &Dir) -> io::Result<u64> {
    let mut status_block = MaybeUninit::<IO_STATUS_BLOCK>::uninit();
    let mut info = MaybeUninit::<FILE_FS_FULL_SIZE_INFORMATION>::uninit();
    // SAFETY: all pointers refer to properly aligned, writable buffers of the
    // declared sizes; the borrowed directory handle stays valid for the call.
    let status = unsafe {
        NtQueryVolumeInformationFile(
            raw_handle(parent),
            status_block.as_mut_ptr(),
            info.as_mut_ptr().cast::<c_void>(),
            size_of::<FILE_FS_FULL_SIZE_INFORMATION>() as u32,
            FileFsFullSizeInformation,
        )
    };
    if status != 0 {
        // SAFETY: this pure ntdll conversion accepts the returned NTSTATUS.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    // SAFETY: successful synchronous query initialized the output structure.
    let info = unsafe { info.assume_init() };
    if info.SectorsPerAllocationUnit == 0 || info.BytesPerSector == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "volume reported a zero allocation unit size",
        ));
    }
    let units = u64::try_from(info.CallerAvailableAllocationUnits)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative available space"))?;
    units
        .checked_mul(u64::from(info.SectorsPerAllocationUnit))
        .and_then(|bytes| bytes.checked_mul(u64::from(info.BytesPerSector)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "available space overflow"))
}

#[derive(Debug)]
pub struct PinnedParentPath {
    _handles: Vec<File>,
}

/// Pins a freshly-created sibling directory while path-based writers use it.
/// Keep a `PinnedParentPath` alive as well, so ancestors cannot be renamed.
#[derive(Debug)]
pub struct StageGuard {
    parent: Dir,
    dir: Option<Dir>,
    path: PathBuf,
    leaf: OsString,
    identity: FileIdentity,
}

struct StageAcl {
    descriptor: SECURITY_DESCRIPTOR,
    _acl_storage: Vec<u32>,
    user_sid: Vec<u32>,
}

impl StageGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dir(&self) -> &Dir {
        self.dir.as_ref().expect("stage remains open")
    }

    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Close the pin and publish the same stage without replacing a destination.
    pub fn promote(mut self, destination_leaf: &OsStr) -> io::Result<()> {
        drop(self.dir.take());
        rename_directory_noreplace(&self.parent, &self.leaf, destination_leaf, self.identity)
    }

    /// Close the pin and remove this exact empty stage.
    pub fn remove_empty(mut self) -> io::Result<()> {
        drop(self.dir.take());
        remove_directory_if_identity(&self.parent, &self.leaf, self.identity)
    }
}

pub fn create_pinned_stage(parent: &Dir, leaf: &OsStr) -> io::Result<StageGuard> {
    use fs_at::os::windows::FileExt;
    let leaf = literal_leaf(leaf)?;
    require_local_fixed_volume(parent)?;
    require_persistent_acls(parent)?;
    let acl = private_stage_acl()?;
    let created = create_stage_handle(parent, leaf, &acl)?;
    let result = (|| {
        let identity = file_identity(&created)?;
        let path = final_path(&created)?;
        verify_private_stage_acl(&created, &acl.user_sid)?;
        Ok(StageGuard {
            parent: parent.try_clone()?,
            dir: Some(Dir::from_std_file(created.try_clone()?)),
            path,
            leaf: leaf.to_os_string(),
            identity,
        })
    })();
    if result.is_err() {
        // Delete the just-created object by handle, even if its name changed.
        let _ = created.delete_by_handle();
    }
    result
}

fn create_stage_handle(parent: &Dir, leaf: &OsStr, acl: &StageAcl) -> io::Result<File> {
    use fs_at::os::windows::FileExt;
    let mut wide: Vec<u16> = leaf.encode_wide().collect();
    let name_bytes = wide
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| invalid_path("stage leaf is too long for a UNICODE_STRING"))?;
    wide.push(0);
    let name = UNICODE_STRING {
        Length: name_bytes,
        MaximumLength: name_bytes + size_of::<u16>() as u16,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: raw_handle(parent),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE as u32,
        SecurityDescriptor: (&acl.descriptor as *const SECURITY_DESCRIPTOR).cast::<c_void>(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle: HANDLE = ptr::null_mut();
    let mut status_block = MaybeUninit::<IO_STATUS_BLOCK>::uninit();
    // SAFETY: the validated literal leaf is a live, terminated UTF-16 buffer;
    // the OBJECT_ATTRIBUTES refers to the live bound parent and the absolute
    // security descriptor, whose ACL storage remains live in StageAcl. The
    // newly created directory handle is the pin itself: no second open occurs.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            DELETE
                | FILE_ADD_FILE
                | FILE_ADD_SUBDIRECTORY
                | FILE_LIST_DIRECTORY
                | FILE_TRAVERSE
                | FILE_READ_ATTRIBUTES
                | FILE_WRITE_ATTRIBUTES
                | READ_CONTROL
                | SYNCHRONIZE,
            &attributes,
            status_block.as_mut_ptr(),
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_CREATE,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status != 0 {
        // SAFETY: this pure ntdll conversion accepts the returned NTSTATUS.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "NtCreateFile returned an invalid stage handle",
        ));
    }
    // SAFETY: a successful NtCreateFile returned this newly owned handle.
    let file = unsafe { File::from_raw_handle(handle) };
    // SAFETY: the synchronous creation call initialized IO_STATUS_BLOCK.
    if unsafe { status_block.assume_init() }.Information as u32 != FILE_CREATED {
        let _ = file.delete_by_handle();
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stage creation did not create a new directory",
        ));
    }
    Ok(file)
}

pub fn pin_requested_parent_path(path: &Path, expected_dir: &Dir) -> io::Result<PinnedParentPath> {
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(invalid_path("expected an absolute Windows path"));
    };
    if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "UNC and device output paths are unsupported",
        ));
    }
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(invalid_path("expected an absolute Windows path"));
    }
    let mut root = OsString::from(prefix.as_os_str());
    root.push("\\");
    let mut current = PathBuf::from(root);
    let mut handles = vec![open_pinned_directory(&current, false)?];
    for component in components {
        let Component::Normal(leaf) = component else {
            return Err(invalid_path("parent traversal is not allowed"));
        };
        literal_leaf(leaf)?;
        current.push(leaf);
        handles.push(open_pinned_directory(&current, false)?);
    }
    if file_identity(handles.last().expect("root handle exists"))? != file_identity(expected_dir)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "requested parent identity changed",
        ));
    }
    require_local_fixed_volume(expected_dir)?;
    Ok(PinnedParentPath { _handles: handles })
}

fn raw_handle(handle: &impl AsHandle) -> HANDLE {
    handle.as_handle().as_raw_handle() as HANDLE
}

fn require_local_fixed_volume(handle: &impl AsHandle) -> io::Result<()> {
    let mut status_block = MaybeUninit::<IO_STATUS_BLOCK>::uninit();
    let mut device = MaybeUninit::<FILE_FS_DEVICE_INFORMATION>::uninit();
    // SAFETY: the volume query is bound to the live directory handle, and
    // both output buffers have the exact alignment and size required.
    let status = unsafe {
        NtQueryVolumeInformationFile(
            raw_handle(handle),
            status_block.as_mut_ptr(),
            device.as_mut_ptr().cast::<c_void>(),
            size_of::<FILE_FS_DEVICE_INFORMATION>() as u32,
            FileFsDeviceInformation,
        )
    };
    if status != 0 {
        // SAFETY: this pure ntdll conversion accepts the returned NTSTATUS.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    // SAFETY: a successful query initialized FILE_FS_DEVICE_INFORMATION.
    if unsafe { device.assume_init() }.Characteristics & FILE_REMOTE_DEVICE != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "network volumes are unsupported for bound publication",
        ));
    }
    let path = final_path(handle)?;
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Err(invalid_path("directory has no Windows volume prefix"));
    };
    let letter = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "UNC and device volumes are unsupported",
            ));
        }
    };
    let root = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
    // SAFETY: root is a terminated UTF-16 drive-root path and lives throughout
    // this query. Only a fixed local drive is accepted for bound publication.
    if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stage volume is not a fixed local drive",
        ));
    }
    Ok(())
}

fn require_persistent_acls(handle: &impl AsHandle) -> io::Result<()> {
    let mut flags = 0_u32;
    // SAFETY: the handle remains open, unused output fields are null, and
    // `flags` is a writable u32 for the filesystem feature flags.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            raw_handle(handle),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut flags,
            ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & FILE_PERSISTENT_ACLS == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stage volume does not enforce persistent ACLs",
        ));
    }
    Ok(())
}

fn private_stage_acl() -> io::Result<StageAcl> {
    let mut token_raw: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a process pseudo-handle and token_raw
    // receives a newly owned token handle on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned a fresh owned handle, closed on drop.
    let token = unsafe { OwnedHandle::from_raw_handle(token_raw) };
    let mut needed = 0_u32;
    // SAFETY: this size probe passes no output buffer and receives its required size.
    unsafe {
        GetTokenInformation(
            raw_handle(&token),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if (needed as usize) < size_of::<TOKEN_USER>() || needed > 64 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid token user buffer size",
        ));
    }
    let mut token_storage = vec![0_u64; (needed as usize).div_ceil(size_of::<u64>())];
    // SAFETY: token_storage is 8-byte aligned and holds at least `needed`
    // writable bytes; TOKEN_USER is read only after this call succeeds.
    if unsafe {
        GetTokenInformation(
            raw_handle(&token),
            TokenUser,
            token_storage.as_mut_ptr().cast::<c_void>(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful TokenUser query initialized the aligned buffer.
    let user = unsafe { &*token_storage.as_ptr().cast::<TOKEN_USER>() };
    let sid = user.User.Sid;
    // SAFETY: TokenUser supplies a SID pointer within the live token buffer.
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid token user SID",
        ));
    }
    // SAFETY: the SID was checked as valid and the token buffer is still live.
    let sid_len = unsafe { GetLengthSid(sid) } as usize;
    if sid_len == 0 || sid_len > 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid user SID length",
        ));
    }
    let mut user_sid = vec![0_u32; sid_len.div_ceil(size_of::<u32>())];
    // SAFETY: both byte ranges are live, nonoverlapping, and sid_len is the
    // validated size returned by GetLengthSid.
    unsafe {
        ptr::copy_nonoverlapping(
            sid.cast::<u8>(),
            user_sid.as_mut_ptr().cast::<u8>(),
            sid_len,
        );
    }
    let acl_bytes = size_of::<ACL>()
        .checked_add(size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>())
        .and_then(|n| n.checked_add(sid_len))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ACL size overflow"))?;
    let mut acl_storage = vec![0_u32; acl_bytes.div_ceil(size_of::<u32>())];
    let acl_ptr = acl_storage.as_mut_ptr().cast::<ACL>();
    // SAFETY: acl_storage is writable and 4-byte aligned for the declared ACL
    // capacity. AddAccessAllowedAceEx copies the validated SID into that ACL.
    unsafe {
        if InitializeAcl(
            acl_ptr,
            (acl_storage.len() * size_of::<u32>()) as u32,
            ACL_REVISION,
        ) == 0
            || AddAccessAllowedAceEx(
                acl_ptr,
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                FILE_ALL_ACCESS,
                user_sid.as_mut_ptr().cast::<c_void>(),
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let mut descriptor = MaybeUninit::<SECURITY_DESCRIPTOR>::uninit();
    // SAFETY: the descriptor output has the required size and alignment;
    // `acl_ptr` remains live through the later NtCreateFile call.
    unsafe {
        if InitializeSecurityDescriptor(
            descriptor.as_mut_ptr().cast::<c_void>(),
            SECURITY_DESCRIPTOR_REVISION,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: InitializeSecurityDescriptor initialized this absolute descriptor.
    let mut descriptor = unsafe { descriptor.assume_init() };
    // SAFETY: a present, non-null ACL is required; marking it protected stops
    // inherited parent ACEs from broadening access to the stage.
    unsafe {
        if SetSecurityDescriptorDacl(
            (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast::<c_void>(),
            1,
            acl_ptr,
            0,
        ) == 0
            || SetSecurityDescriptorControl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast::<c_void>(),
                SE_DACL_PROTECTED,
                SE_DACL_PROTECTED,
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(StageAcl {
        descriptor,
        _acl_storage: acl_storage,
        user_sid,
    })
}

fn verify_private_stage_acl(stage: &impl AsHandle, expected_sid: &[u32]) -> io::Result<()> {
    let mut needed = 0_u32;
    // SAFETY: this size probe requests only the DACL and provides no output
    // buffer. A subsequent call uses the returned size.
    unsafe {
        GetKernelObjectSecurity(
            raw_handle(stage),
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if (needed as usize) < size_of::<SECURITY_DESCRIPTOR>() || needed > 64 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid stage ACL size",
        ));
    }
    let mut storage = vec![0_u64; (needed as usize).div_ceil(size_of::<u64>())];
    let sd = storage.as_mut_ptr().cast::<c_void>();
    // SAFETY: storage is aligned and holds at least `needed` writable bytes;
    // the stage handle has READ_CONTROL and remains live for the query.
    if unsafe {
        GetKernelObjectSecurity(
            raw_handle(stage),
            DACL_SECURITY_INFORMATION,
            sd,
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut control = 0_u16;
    let mut revision = 0_u32;
    let mut present = 0_i32;
    let mut defaulted = 0_i32;
    let mut acl_ptr: *mut ACL = ptr::null_mut();
    // SAFETY: the query above returned a valid self-relative descriptor in
    // storage, and each output pointer is valid for its documented type.
    unsafe {
        if GetSecurityDescriptorControl(sd, &mut control, &mut revision) == 0
            || GetSecurityDescriptorDacl(sd, &mut present, &mut acl_ptr, &mut defaulted) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    if control & SE_DACL_PROTECTED == 0 || present == 0 || acl_ptr.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "stage DACL is not protected",
        ));
    }
    let mut acl_info = MaybeUninit::<ACL_SIZE_INFORMATION>::uninit();
    // SAFETY: acl_ptr points inside the live descriptor buffer, and acl_info
    // has the required size and alignment for AclSizeInformation.
    if unsafe {
        GetAclInformation(
            acl_ptr,
            acl_info.as_mut_ptr().cast::<c_void>(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful query initialized ACL_SIZE_INFORMATION.
    if unsafe { acl_info.assume_init() }.AceCount != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "stage DACL has unexpected ACEs",
        ));
    }
    let mut ace_ptr: *mut c_void = ptr::null_mut();
    // SAFETY: ACL has exactly one ACE; GetAce returns a pointer within its
    // live descriptor buffer, which is then inspected as ACCESS_ALLOWED_ACE.
    if unsafe { GetAce(acl_ptr, 0, &mut ace_ptr) } == 0 || ace_ptr.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetAce returned the first ACE, whose header fits within the ACL.
    let header = unsafe { &*ace_ptr.cast::<ACE_HEADER>() };
    if header.AceType != 0 || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "stage ACE type is unexpected",
        ));
    }
    // SAFETY: the checked type and size cover the ACCESS_ALLOWED_ACE fields.
    let ace = unsafe { &*ace_ptr.cast::<ACCESS_ALLOWED_ACE>() };
    let expected_flags = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
    if ace.Header.AceFlags & expected_flags != expected_flags
        || ace.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "stage ACE is not private",
        ));
    }
    let sid_ptr = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
    let sid_capacity = usize::from(ace.Header.AceSize) - offset_of!(ACCESS_ALLOWED_ACE, SidStart);
    if sid_capacity < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stage ACE SID is truncated",
        ));
    }
    // SAFETY: the ACE contains at least the fixed SID header; GetLengthSid
    // reads that header to determine the full SID size.
    let sid_length = unsafe { GetLengthSid(sid_ptr) } as usize;
    if sid_length > sid_capacity || sid_length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stage ACE SID exceeds its buffer",
        ));
    }
    // SAFETY: the ACE is an access-allowed ACE and its SID begins at SidStart;
    // expected_sid is an aligned copy of the validated current-user SID.
    if unsafe { EqualSid(sid_ptr, expected_sid.as_ptr().cast_mut().cast::<c_void>()) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "stage owner ACE changed",
        ));
    }
    Ok(())
}

fn invalid_path(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn literal_leaf(leaf: &OsStr) -> io::Result<&OsStr> {
    let units: Vec<u16> = leaf.encode_wide().collect();
    if units.is_empty() || units.len() > 255 {
        return Err(invalid_path("leaf length is outside Windows limits"));
    }
    let value = String::from_utf16(&units).map_err(|_| invalid_path("invalid UTF-16 leaf"))?;
    if value == "."
        || value == ".."
        || value.ends_with([' ', '.'])
        || value.chars().any(|ch| {
            ch <= '\u{1f}' || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
    {
        return Err(invalid_path("leaf is not a literal Windows file name"));
    }
    let stem = value.split('.').next().unwrap_or("").to_ascii_uppercase();
    let numbered_device = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered_device
    {
        return Err(invalid_path("reserved Windows device name"));
    }
    Ok(leaf)
}

fn open_source_for_delete(parent: &Dir, leaf: &OsStr) -> io::Result<File> {
    use fs_at::os::windows::OpenOptionsExt;
    let parent_file = File::from(parent.as_handle().try_clone_to_owned()?);
    let mut options = fs_at::OpenOptions::default();
    options
        .follow(false)
        .desired_access(DELETE | FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE);
    let source = options.open_dir_at(&parent_file, Path::new(leaf))?;
    check_plain_directory(&source)?;
    Ok(source)
}

fn check_plain_directory(handle: &impl AsHandle) -> io::Result<()> {
    let mut info = MaybeUninit::<FILE_ATTRIBUTE_TAG_INFO>::uninit();
    // SAFETY: valid borrowed handle and correctly sized/aligned output buffer.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            raw_handle(handle),
            FileAttributeTagInfo,
            info.as_mut_ptr().cast::<c_void>(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful query initialized FILE_ATTRIBUTE_TAG_INFO.
    let info = unsafe { info.assume_init() };
    if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(invalid_path("source is not a plain directory"));
    }
    Ok(())
}

fn final_path(handle: &impl AsHandle) -> io::Result<PathBuf> {
    let mut wide = vec![0_u16; 1024];
    loop {
        // SAFETY: the borrowed handle remains valid and `wide` is a writable
        // UTF-16 buffer of exactly the capacity supplied to the API.
        let count = unsafe {
            GetFinalPathNameByHandleW(raw_handle(handle), wide.as_mut_ptr(), wide.len() as u32, 0)
        };
        if count == 0 {
            return Err(io::Error::last_os_error());
        }
        if (count as usize) < wide.len() {
            wide.truncate(count as usize);
            return Ok(PathBuf::from(OsString::from_wide(&wide)));
        }
        if count > 32_767 {
            return Err(invalid_path("directory path is too long"));
        }
        wide.resize(count as usize + 1, 0);
    }
}

fn open_pinned_directory(path: &Path, writable: bool) -> io::Result<File> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut access =
        FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY | FILE_TRAVERSE | READ_CONTROL | SYNCHRONIZE;
    if writable {
        access |= FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | FILE_WRITE_ATTRIBUTES;
    }
    // SAFETY: the UTF-16 path is terminated, flags open the named directory
    // itself, and File::from_raw_handle takes sole ownership of a new handle.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a fresh owned handle, checked above.
    let file = unsafe { File::from_raw_handle(handle) };
    check_plain_directory(&file)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std::ambient_authority;
    use std::{fs, path::PathBuf, process::Command};

    fn opened(path: &Path) -> Dir {
        Dir::open_ambient_dir(path, ambient_authority()).unwrap()
    }

    #[test]
    fn identity_tracks_the_open_object_including_128_bit_file_id() {
        let root = tempfile::tempdir().unwrap();
        let one = root.path().join("one");
        let two = root.path().join("two");
        fs::write(&one, b"one").unwrap();
        fs::write(&two, b"two").unwrap();
        let first = File::open(&one).unwrap();
        let reopened = File::open(&one).unwrap();
        let other = File::open(&two).unwrap();
        assert_eq!(
            file_identity(&first).unwrap(),
            file_identity(&reopened).unwrap()
        );
        assert_ne!(
            file_identity(&first).unwrap(),
            file_identity(&other).unwrap()
        );
    }

    #[test]
    fn rename_preserves_existing_destination_and_promotes_exact_source() {
        let root = tempfile::tempdir().unwrap();
        let parent = opened(root.path());
        fs::create_dir(root.path().join("source")).unwrap();
        fs::create_dir(root.path().join("occupied")).unwrap();
        fs::write(root.path().join("occupied/marker"), b"keep").unwrap();
        let source = parent.open_dir("source").unwrap();
        let expected = file_identity(&source).unwrap();
        drop(source);
        assert!(
            rename_directory_noreplace(
                &parent,
                OsStr::new("source"),
                OsStr::new("occupied"),
                expected,
            )
            .is_err()
        );
        assert_eq!(
            fs::read(root.path().join("occupied/marker")).unwrap(),
            b"keep"
        );
        assert!(root.path().join("source").is_dir());
        rename_directory_noreplace(
            &parent,
            OsStr::new("source"),
            OsStr::new("promoted"),
            expected,
        )
        .unwrap();
        assert!(!root.path().join("source").exists());
        assert_eq!(
            file_identity(&parent.open_dir("promoted").unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn nt_relative_rename_accepts_a_one_character_destination() {
        let root = tempfile::tempdir().unwrap();
        let parent = opened(root.path());
        fs::create_dir(root.path().join("s")).unwrap();
        let expected = file_identity(&parent.open_dir("s").unwrap()).unwrap();
        rename_directory_noreplace(&parent, OsStr::new("s"), OsStr::new("x"), expected).unwrap();
        assert!(!root.path().join("s").exists());
        assert_eq!(
            file_identity(&parent.open_dir("x").unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn rename_rejects_substituted_source_and_nonliteral_leaf() {
        let root = tempfile::tempdir().unwrap();
        let parent = opened(root.path());
        fs::create_dir(root.path().join("source")).unwrap();
        fs::create_dir(root.path().join("other")).unwrap();
        let wrong = file_identity(&parent.open_dir("other").unwrap()).unwrap();
        assert!(
            rename_directory_noreplace(
                &parent,
                OsStr::new("source"),
                OsStr::new("published"),
                wrong,
            )
            .is_err()
        );
        assert!(
            rename_directory_noreplace(
                &parent,
                OsStr::new("source"),
                OsStr::new("subdir\\published"),
                file_identity(&parent.open_dir("source").unwrap()).unwrap(),
            )
            .is_err()
        );
        assert!(root.path().join("source").is_dir());
    }

    #[test]
    fn removal_deletes_only_the_expected_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        let parent = opened(root.path());
        fs::create_dir(root.path().join("owned")).unwrap();
        fs::create_dir(root.path().join("other")).unwrap();
        let owned = file_identity(&parent.open_dir("owned").unwrap()).unwrap();
        let other = file_identity(&parent.open_dir("other").unwrap()).unwrap();
        assert!(remove_directory_if_identity(&parent, OsStr::new("owned"), other).is_err());
        assert!(root.path().join("owned").is_dir());
        remove_directory_if_identity(&parent, OsStr::new("owned"), owned).unwrap();
        assert!(!root.path().join("owned").exists());
        assert!(root.path().join("other").is_dir());
    }

    #[test]
    fn pinned_parent_prevents_ancestor_replacement() {
        let root = tempfile::tempdir().unwrap();
        let parent_path: PathBuf = root.path().join("parent");
        fs::create_dir(&parent_path).unwrap();
        let expected = opened(&parent_path);
        let guard = pin_requested_parent_path(&parent_path, &expected).unwrap();
        drop(expected);
        assert!(fs::rename(&parent_path, root.path().join("moved")).is_err());
        drop(guard);
        fs::rename(&parent_path, root.path().join("moved")).unwrap();
    }

    #[test]
    fn pinned_parent_rejects_a_different_final_directory() {
        let root = tempfile::tempdir().unwrap();
        let named = root.path().join("named");
        let other = root.path().join("other");
        fs::create_dir(&named).unwrap();
        fs::create_dir(&other).unwrap();
        assert!(pin_requested_parent_path(&named, &opened(&other)).is_err());
    }

    #[test]
    fn requested_unc_parent_is_not_accepted_for_publication() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            pin_requested_parent_path(Path::new(r"\\server\share\dataset"), &opened(root.path()))
                .is_err()
        );
    }

    #[test]
    fn available_space_is_queried_from_bound_directory() {
        let root = tempfile::tempdir().unwrap();
        assert!(available_space(&opened(root.path())).unwrap() > 0);
    }

    #[test]
    fn created_stage_stays_pinned_through_write_and_promotes_without_clobber() {
        let root = tempfile::tempdir().unwrap();
        let parent = opened(root.path());
        let stage = create_pinned_stage(&parent, OsStr::new(".mgwt-stage-test")).unwrap();
        assert_eq!(file_identity(stage.dir()).unwrap(), stage.identity());
        fs::write(stage.path().join("marker"), b"verified").unwrap();
        assert!(fs::rename(stage.path(), root.path().join("stolen")).is_err());
        stage.promote(OsStr::new("dataset")).unwrap();
        assert_eq!(
            fs::read(root.path().join("dataset/marker")).unwrap(),
            b"verified"
        );
    }

    #[test]
    fn created_empty_stage_can_be_removed_by_its_identity() {
        let root = tempfile::tempdir().unwrap();
        let parent = opened(root.path());
        let stage = create_pinned_stage(&parent, OsStr::new(".mgwt-stage-empty")).unwrap();
        let path = stage.path().to_path_buf();
        stage.remove_empty().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn stage_acl_does_not_inherit_an_everyone_grant_from_parent() {
        let root = tempfile::tempdir().unwrap();
        let status = Command::new("icacls")
            .arg(root.path())
            .arg("/grant")
            .arg("*S-1-1-0:(OI)(CI)F")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "icacls must make the disposable parent permissive"
        );
        let parent = opened(root.path());
        let stage = create_pinned_stage(&parent, OsStr::new(".mgwt-private-test")).unwrap();
        let sid = private_stage_acl().unwrap();
        verify_private_stage_acl(stage.dir(), &sid.user_sid).unwrap();
        stage.remove_empty().unwrap();
    }
}
