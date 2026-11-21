//! File security for rollbacks: a file's owner, group and DACL as SDDL,
//! putting them back, replacing a file in place, and the helper's private
//! snapshot folder.

use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW, SetNamedSecurityInfoW,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetSecurityDescriptorGroup, GetSecurityDescriptorOwner,
    ACL, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

use super::acl::{self, Sddl};
use super::{from_pwstr, wide, Local};

/// The DACL of the helper's snapshot folder: SYSTEM and Administrators only.
pub const PRIVATE_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// The file's owner, group and DACL as SDDL (`O:…G:…D:…`).
pub fn sddl_of(path: &Path) -> io::Result<String> {
    let w = wide(path.as_os_str());
    let info = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: valid path; Windows allocates the descriptor, freed below.
    let r = unsafe {
        GetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            info,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    let _sd = Local(sd);
    let mut s: *mut u16 = std::ptr::null_mut();
    // SAFETY: a valid descriptor; Windows allocates the string.
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, SDDL_REVISION_1, info, &mut s, std::ptr::null_mut())
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let _s = Local(s.cast());
    // SAFETY: a NUL-terminated string from Windows.
    Ok(unsafe { from_pwstr(s) })
}

fn set(path: &Path, info: u32, owner: PSID, group: PSID, dacl: *mut ACL) -> io::Result<()> {
    let w = wide(path.as_os_str());
    // SAFETY: valid path; the pointers are null or point into a live descriptor.
    let r = unsafe { SetNamedSecurityInfoW(w.as_ptr(), SE_FILE_OBJECT, info, owner, group, dacl, std::ptr::null()) };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    Ok(())
}

/// Gives the file the DACL (protected or not, as saved) and, where the
/// helper may, the owner and group from `sddl`. Setting someone else as
/// owner takes a privilege the helper doesn't hold, so then only the DACL
/// is put back. Returns whether the owner was put back too.
pub fn apply(path: &Path, sddl: &str) -> io::Result<bool> {
    let sd = Sddl::new(sddl)?;
    let d = sd.descriptor();
    let (mut owner, mut group): (PSID, PSID) = (std::ptr::null_mut(), std::ptr::null_mut());
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let (mut present, mut defaulted) = (0, 0);
    let (mut control, mut rev) = (0u16, 0u32);
    // SAFETY: a valid descriptor; the out-pointers point into it.
    let ok = unsafe {
        GetSecurityDescriptorOwner(d, &mut owner, &mut defaulted) != 0
            && GetSecurityDescriptorGroup(d, &mut group, &mut defaulted) != 0
            && GetSecurityDescriptorDacl(d, &mut present, &mut dacl, &mut defaulted) != 0
            && GetSecurityDescriptorControl(d, &mut control, &mut rev) != 0
    };
    if !ok {
        return Err(io::Error::last_os_error());
    }
    if present == 0 || dacl.is_null() {
        return Err(io::Error::other("the saved descriptor has no DACL"));
    }
    let protect = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    let all = DACL_SECURITY_INFORMATION | protect | OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION;
    if !owner.is_null() && !group.is_null() && set(path, all, owner, group, dacl).is_ok() {
        return Ok(true);
    }
    set(path, DACL_SECURITY_INFORMATION | protect, std::ptr::null_mut(), std::ptr::null_mut(), dacl)?;
    Ok(false)
}

/// Puts `with`'s contents in place of `target`, keeping `target`'s identity
/// (ACL, owner, attributes, hard links) like an in-place save does.
pub fn replace(target: &Path, with: &Path) -> io::Result<()> {
    let (t, w) = (wide(target.as_os_str()), wide(with.as_os_str()));
    // SAFETY: valid paths; no backup, no reserved arguments.
    let ok = unsafe { ReplaceFileW(t.as_ptr(), w.as_ptr(), std::ptr::null(), 0, std::ptr::null(), std::ptr::null()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The owner's SID of a file or folder.
fn owner_of(path: &Path) -> io::Result<String> {
    let w = wide(path.as_os_str());
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: valid path; `owner` points into `sd`, freed below.
    let r = unsafe {
        GetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    // SAFETY: owner points into the live descriptor.
    let s = unsafe { acl::sid_string(owner) };
    // SAFETY: allocated by GetNamedSecurityInfoW.
    unsafe { LocalFree(sd) };
    s.ok_or_else(|| io::Error::other("no owner"))
}

/// Creates the snapshot folder (or takes the existing one) with a DACL that
/// lets only SYSTEM and Administrators in. Anyone may create folders in
/// `ProgramData`, so a folder someone else owns (made there first) is
/// refused rather than used.
pub fn private_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let md = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !md.is_dir() {
        return Err(format!("{}: not a folder", path.display()));
    }
    let owner = owner_of(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if owner != acl::SYSTEM && owner != acl::ADMINISTRATORS {
        return Err(format!("{} belongs to {owner}, not SYSTEM or Administrators; remove it", path.display()));
    }
    let sd = Sddl::new(PRIVATE_SDDL).map_err(|e| e.to_string())?;
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: a valid descriptor; `dacl` points into it.
    if unsafe { GetSecurityDescriptorDacl(sd.descriptor(), &mut present, &mut dacl, &mut defaulted) } == 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    set(
        path,
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        dacl,
    )
    .map_err(|e| format!("set the ACL of {}: {e}", path.display()))
}
