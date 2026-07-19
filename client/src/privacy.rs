use std::path::Path;

use anyhow::Result;

#[cfg(unix)]
pub fn restrict_file(path: &Path) -> Result<()> {
    use std::{fs, os::unix::fs::PermissionsExt};

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(unix)]
pub fn restrict_dir(path: &Path) -> Result<()> {
    use std::{fs, os::unix::fs::PermissionsExt};

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(windows)]
pub fn restrict_file(path: &Path) -> Result<()> {
    windows_acl::restrict(path, false)
}

#[cfg(windows)]
pub fn restrict_dir(path: &Path) -> Result<()> {
    windows_acl::restrict(path, true)
}

#[cfg(windows)]
pub fn restrict_state_tree(root: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let metadata = entry.path().symlink_metadata()?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            anyhow::bail!(
                "private Windows state contains a reparse point at {}; move neuro-sync state to a local directory without links or junctions",
                entry.path().display()
            );
        }
        if metadata.is_dir() {
            restrict_dir(entry.path())?;
        } else if metadata.is_file() {
            restrict_file(entry.path())?;
        } else {
            anyhow::bail!(
                "private Windows state contains an unsupported object at {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn restrict_state_tree(_root: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub fn restrict_file(path: &Path) -> Result<()> {
    anyhow::bail!(
        "cannot secure private neuro-sync state file {} on this operating system",
        path.display()
    )
}

#[cfg(not(any(unix, windows)))]
pub fn restrict_dir(path: &Path) -> Result<()> {
    anyhow::bail!(
        "cannot secure private neuro-sync state directory {} on this operating system",
        path.display()
    )
}

#[cfg(windows)]
mod windows_acl {
    use std::{
        io,
        mem::{size_of, zeroed},
        os::windows::ffi::OsStrExt,
        path::Path,
        ptr::{null, null_mut},
    };

    use anyhow::{Result, bail};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, GENERIC_ALL, HANDLE, LocalFree},
        Security::Authorization::{
            EXPLICIT_ACCESS_W, GRANT_ACCESS, GetExplicitEntriesFromAclW, GetNamedSecurityInfoW,
            SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
            TRUSTEE_IS_USER,
        },
        Security::{
            CopySid, DACL_SECURITY_INFORMATION, EqualSid, GetLengthSid,
            GetSecurityDescriptorControl, GetTokenInformation, NO_INHERITANCE,
            PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: this wrapper owns the successful OpenProcessToken handle.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct LocalMemory(*mut core::ffi::c_void);

    impl Drop for LocalMemory {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: Windows allocated these buffers with LocalAlloc.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    fn acl_failure(path: &Path, action: &str) -> anyhow::Error {
        let source = io::Error::last_os_error();
        anyhow::anyhow!(
            "could not {action} private Windows ACLs for {} ({source}); use a local ACL-capable state directory owned by this Windows account",
            path.display()
        )
    }

    fn acl_result_failure(path: &Path, action: &str, result: u32) -> anyhow::Error {
        let source = io::Error::from_raw_os_error(result as i32);
        anyhow::anyhow!(
            "could not {action} private Windows ACLs for {} ({source}); use a local ACL-capable state directory owned by this Windows account",
            path.display()
        )
    }

    fn acl_mismatch(path: &Path, detail: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "Windows did not retain private neuro-sync ACLs for {} ({detail}); use a local ACL-capable state directory owned by this Windows account",
            path.display()
        )
    }

    fn wide_path(path: &Path) -> Result<Vec<u16>> {
        let mut value = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if value.contains(&0) {
            bail!("private Windows state path contains an invalid NUL character");
        }
        value.push(0);
        Ok(value)
    }

    fn current_user_sid(path: &Path) -> Result<Vec<usize>> {
        let mut raw_token: HANDLE = null_mut();
        // SAFETY: raw_token is a valid out pointer and the pseudo process handle is valid.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
            return Err(acl_failure(path, "read the current account for"));
        }
        let token = OwnedHandle(raw_token);
        let mut required = 0_u32;
        // SAFETY: the documented sizing call uses a null buffer and zero length.
        let sizing_result =
            unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required) };
        if sizing_result != 0
            || required == 0
            || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
        {
            return Err(acl_failure(path, "size the current-account SID for"));
        }
        let word_size = size_of::<usize>();
        let mut token_information = vec![0_usize; (required as usize).div_ceil(word_size)];
        // SAFETY: the aligned allocation is at least required bytes and remains live below.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                token_information.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(acl_failure(path, "read the current-account SID for"));
        }
        // SAFETY: GetTokenInformation populated a TOKEN_USER at the aligned buffer start.
        let source_sid = unsafe {
            (*(token_information.as_ptr().cast::<TOKEN_USER>()))
                .User
                .Sid
        };
        // SAFETY: source_sid comes from the successful TOKEN_USER result.
        let sid_length = unsafe { GetLengthSid(source_sid) };
        if sid_length == 0 {
            return Err(acl_failure(path, "measure the current-account SID for"));
        }
        let mut sid = vec![0_usize; (sid_length as usize).div_ceil(word_size)];
        // SAFETY: the aligned SID buffer is at least sid_length bytes and source_sid is still live.
        if unsafe { CopySid(sid_length, sid.as_mut_ptr().cast(), source_sid) } == 0 {
            return Err(acl_failure(path, "copy the current-account SID for"));
        }
        Ok(sid)
    }

    pub(super) fn restrict(path: &Path, directory: bool) -> Result<()> {
        let wide = wide_path(path)?;
        let mut sid = current_user_sid(path)?;
        let inheritance = if directory {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        };
        // SAFETY: every field is initialized below before the value is passed to Windows.
        let mut access: EXPLICIT_ACCESS_W = unsafe { zeroed() };
        access.grfAccessPermissions = GENERIC_ALL;
        access.grfAccessMode = SET_ACCESS;
        access.grfInheritance = inheritance;
        access.Trustee.TrusteeForm = TRUSTEE_IS_SID;
        access.Trustee.TrusteeType = TRUSTEE_IS_USER;
        access.Trustee.ptstrName = sid.as_mut_ptr().cast();
        let mut acl = null_mut();
        // SAFETY: access points at the live SID, old ACL is intentionally empty, and acl is an out pointer.
        let result = unsafe { SetEntriesInAclW(1, &access, null(), &mut acl) };
        let owned_acl = LocalMemory(acl.cast());
        if result != 0 {
            return Err(acl_result_failure(path, "construct", result));
        }
        if acl.is_null() {
            return Err(acl_mismatch(path, "ACL construction returned no DACL"));
        }
        // SAFETY: wide is NUL terminated and owned_acl contains a valid ACL from SetEntriesInAclW.
        let result = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl,
                null(),
            )
        };
        if result != 0 {
            return Err(acl_result_failure(path, "apply", result));
        }
        drop(owned_acl);
        verify(path, &wide, sid.as_mut_ptr().cast(), inheritance)
    }

    fn verify(path: &Path, wide: &[u16], user_sid: PSID, inheritance: u32) -> Result<()> {
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: wide is NUL terminated and all requested output pointers are valid.
        let result = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if result != 0 {
            return Err(acl_result_failure(path, "verify", result));
        }
        if descriptor.is_null() {
            return Err(acl_mismatch(
                path,
                "Windows returned no security descriptor",
            ));
        }
        let owned_descriptor = LocalMemory(descriptor);
        if dacl.is_null() {
            return Err(acl_mismatch(path, "security descriptor returned no DACL"));
        }
        let mut control = 0_u16;
        let mut revision = 0_u32;
        // SAFETY: descriptor is the live security descriptor returned by Windows.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(acl_failure(path, "verify descriptor control for"));
        }
        if control & SE_DACL_PROTECTED == 0 {
            return Err(acl_mismatch(path, "DACL inheritance remained enabled"));
        }
        let mut count = 0_u32;
        let mut entries = null_mut();
        // SAFETY: dacl belongs to the live descriptor and the output pointers are valid.
        let result = unsafe { GetExplicitEntriesFromAclW(dacl, &mut count, &mut entries) };
        let owned_entries = LocalMemory(entries.cast());
        if result != 0 {
            return Err(acl_result_failure(path, "inspect", result));
        }
        if entries.is_null() {
            return Err(acl_mismatch(path, "DACL contains no explicit entry"));
        }
        if count != 1 {
            return Err(acl_mismatch(path, "DACL contains another account"));
        }
        // SAFETY: Windows returned exactly one EXPLICIT_ACCESS_W entry.
        let entry = unsafe { *entries };
        if entry.Trustee.TrusteeForm != TRUSTEE_IS_SID
            || entry.Trustee.ptstrName.is_null()
            // SAFETY: both values are valid SIDs for the duration of this function.
            || unsafe { EqualSid(entry.Trustee.ptstrName.cast(), user_sid) } == 0
            || entry.grfAccessPermissions != GENERIC_ALL
            || !matches!(entry.grfAccessMode, SET_ACCESS | GRANT_ACCESS)
            || entry.grfInheritance != inheritance
        {
            return Err(acl_mismatch(
                path,
                "DACL is not exactly current-account full control",
            ));
        }
        drop(owned_entries);
        drop(owned_descriptor);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_restrictions_are_owner_only() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        fs::create_dir(&directory).unwrap();
        let file = directory.join("secret");
        fs::write(&file, b"secret").unwrap();
        restrict_dir(&directory).unwrap();
        restrict_file(&file).unwrap();
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_restrictions_remove_inheritance_and_other_accounts() {
        use std::fs;

        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        fs::create_dir(&directory).unwrap();
        let file = directory.join("secret");
        fs::write(&file, b"secret").unwrap();
        let existing_directory = directory.join("existing-directory");
        fs::create_dir(&existing_directory).unwrap();
        let inherited = existing_directory.join("existing-before-tree-lockdown");
        fs::write(&inherited, b"private").unwrap();
        restrict_dir(&directory).unwrap();
        restrict_file(&file).unwrap();
        restrict_state_tree(&directory).unwrap();

        let missing = directory.join("missing");
        let error = restrict_file(&missing).unwrap_err().to_string();
        assert!(error.contains("private Windows ACLs"));
        assert!(error.contains("local ACL-capable state directory"));
    }
}
