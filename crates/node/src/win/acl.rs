//! SIDs and security descriptors: who a pipe caller is, and the SDDL the
//! pipe and the installer use.

use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{LookupAccountSidW, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SID_NAME_USE};

use super::{from_pwstr, wide, Local};

/// Well-known SIDs.
pub const SYSTEM: &str = "S-1-5-18";
pub const ADMINISTRATORS: &str = "S-1-5-32-544";

/// `S-1-5-…` for a SID pointer.
///
/// # Safety
/// `sid` is null or points to a valid SID.
pub unsafe fn sid_string(sid: PSID) -> Option<String> {
    if sid.is_null() {
        return None;
    }
    let mut p: *mut u16 = std::ptr::null_mut();
    // SAFETY: the caller promises a valid SID; Windows allocates the string.
    if unsafe { ConvertSidToStringSidW(sid, &mut p) } == 0 {
        return None;
    }
    let _free = Local(p.cast());
    // SAFETY: a NUL-terminated string from Windows.
    Some(unsafe { from_pwstr(p) })
}

/// `DOMAIN\name` for a SID pointer, if it resolves.
///
/// # Safety
/// `sid` points to a valid SID.
pub unsafe fn account_name(sid: PSID) -> Option<String> {
    let (mut n, mut d) = (vec![0u16; 256], vec![0u16; 256]);
    let (mut nl, mut dl) = (n.len() as u32, d.len() as u32);
    let mut use_: SID_NAME_USE = 0;
    // SAFETY: buffers and their lengths match.
    let ok = unsafe {
        LookupAccountSidW(std::ptr::null(), sid, n.as_mut_ptr(), &mut nl, d.as_mut_ptr(), &mut dl, &mut use_)
    };
    if ok == 0 {
        return None;
    }
    let (n, d) = (super::from_wide(&n), super::from_wide(&d));
    Some(if d.is_empty() { n } else { format!("{d}\\{n}") })
}

/// A security descriptor built from SDDL, with SECURITY_ATTRIBUTES pointing
/// at it. Keeps the allocation alive as long as it lives.
pub struct Sddl {
    sd: Local,
    pub attrs: SECURITY_ATTRIBUTES,
}

impl Sddl {
    pub fn new(sddl: &str) -> std::io::Result<Self> {
        let w = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: valid string; Windows allocates the descriptor.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        Ok(Self { sd: Local(sd), attrs })
    }

    pub fn descriptor(&self) -> PSECURITY_DESCRIPTOR {
        self.sd.0
    }
}

/// The DACL of the node's pipe: SYSTEM, Administrators, the service's own
/// SID and the users' group may read and write; nobody else, and nothing
/// from the network (the pipe also rejects remote clients). `group_sid` is
/// the local group's SID, when it exists.
pub fn pipe_sddl(service_sid: Option<&str>, group_sid: Option<&str>, units: &[String]) -> String {
    let mut s = String::from("D:P(A;;GA;;;SY)(A;;GA;;;BA)");
    if let Some(sid) = service_sid.filter(|s| s.starts_with("S-1-")) {
        s.push_str(&format!("(A;;GA;;;{sid})"));
    }
    if let Some(sid) = group_sid.filter(|s| s.starts_with("S-1-")) {
        s.push_str(&format!("(A;;GRGW;;;{sid})"));
    }
    // The node's own units (the check runner), each its own service.
    for sid in units.iter().filter(|s| s.starts_with("S-1-5-80-")) {
        s.push_str(&format!("(A;;GRGW;;;{sid})"));
    }
    s
}

/// The SIDs of the node's own unit services that may use its pipe.
pub fn unit_sids() -> Vec<String> {
    super::unit::ALL.iter().map(|u| u.sid()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_dacl_lists_only_known_principals() {
        assert_eq!(pipe_sddl(None, None, &[]), "D:P(A;;GA;;;SY)(A;;GA;;;BA)");
        assert_eq!(
            pipe_sddl(Some("S-1-5-80-1"), Some("S-1-5-21-9-1001"), &["S-1-5-80-2".into(), "x".into()]),
            "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;S-1-5-80-1)(A;;GRGW;;;S-1-5-21-9-1001)(A;;GRGW;;;S-1-5-80-2)"
        );
        // Anything that isn't a SID string is left out, never pasted in.
        assert_eq!(pipe_sddl(Some("(A;;GA;;;WD)"), Some(""), &[]), "D:P(A;;GA;;;SY)(A;;GA;;;BA)");
        assert!(Sddl::new(&pipe_sddl(None, None, &[])).is_ok());
    }
}
