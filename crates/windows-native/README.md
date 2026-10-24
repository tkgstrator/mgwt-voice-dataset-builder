# Windows filesystem helper

This crate contains the Windows API boundary used by the dataset builder. It is intended for native Windows builds only. The root crate can keep `#![forbid(unsafe_code)]` while this crate audits each Win32 and NT call.

- `file_identity` uses `FileIdInfo` and returns the volume serial plus the full 128-bit file ID. Volumes that report an all-zero ID fail closed.
- `available_space` queries `FileFsFullSizeInformation` through the already-open directory handle. It uses the free allocation units available to the caller, including filesystem quota effects.
- `pin_requested_parent_path` opens every ancestor without following reparse points and without `FILE_SHARE_DELETE`. Keep the returned guard alive during any path-based writes. It compares the final directory with the expected bound directory by full file identity.
- `create_pinned_stage` requires a fixed local drive with persistent ACL support. It creates a new directory relative to the bound parent with a protected DACL granting the current process user full access, then reopens it without `FILE_SHARE_DELETE` and reads back that DACL. The descriptor and ACL memory remain live during the native creation call. Keep both the parent path guard and the stage guard alive while writing and reading back the staged dataset.
- `StageGuard::promote` closes the stage pin, reopens the named source relative to the bound parent with `DELETE` rights and no reparse following, compares full identity, then calls `NtSetInformationFile(FileRenameInformation)` with the bound target parent and `ReplaceIfExists = FALSE`.
- `remove_directory_if_identity` and `StageGuard::remove_empty` delete only the identified empty directory by its handle.

The helper supports fixed local drive paths only. UNC paths, network volumes, removable drives, and volumes without persistent ACL support fail closed. A protected stage DACL prevents access inherited from a permissive parent, but another process running as the same user can still enter the stage. The helper does not recursively remove arbitrary stage contents. Callers must track and remove their own descendants through the pinned stage. If unknown files appear, retain the stage and report the cleanup failure; deleting everything could remove files created by another actor.

The tests cover identity, no-clobber promotion, expected-source mismatch, exact empty-directory cleanup, ancestor pinning, protected stage ACLs under a permissive parent, stage pinning, and free-space queries. These tests must run on a native Windows CI runner; cross-compilation checks alone cannot establish Windows filesystem behavior.
