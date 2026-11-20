//! The few Unix-only std extensions the node uses, under one name on every
//! platform. On Unix this is `std::os::unix` itself. On Windows the same
//! traits exist with Windows meanings, so code that records a file's mode or
//! owner, or asks for a private file, builds unchanged and does the nearest
//! Windows thing (see `docs/WINDOWS.md`): there are no mode bits, so a mode
//! only decides the read-only attribute, and ownership comes from the
//! folder's ACL, which the installer sets.

#[cfg(unix)]
pub use std::os::unix::{ffi, fs};

#[cfg(windows)]
pub mod fs {
    use std::path::Path;

    /// `from_mode` / `mode` on Windows: only the write bits mean anything
    /// (the read-only attribute).
    pub trait PermissionsExt {
        fn from_mode(mode: u32) -> Self;
        fn mode(&self) -> u32;
        fn set_mode(&mut self, mode: u32);
    }

    impl PermissionsExt for std::fs::Permissions {
        fn from_mode(mode: u32) -> Self {
            // Windows has no public constructor; borrow one and set it.
            let mut p = std::env::current_exe()
                .and_then(std::fs::metadata)
                .or_else(|_| std::fs::metadata(std::env::temp_dir()))
                .map(|m| m.permissions())
                .unwrap_or_else(|_| unreachable_permissions());
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(mode & 0o222 == 0);
            p
        }
        fn mode(&self) -> u32 {
            if self.readonly() {
                0o444
            } else {
                0o644
            }
        }
        fn set_mode(&mut self, mode: u32) {
            #[allow(clippy::permissions_set_readonly_false)]
            self.set_readonly(mode & 0o222 == 0);
        }
    }

    #[cold]
    fn unreachable_permissions() -> std::fs::Permissions {
        // Neither the running binary nor the temp folder could be stat'ed;
        // the system root always can.
        std::fs::metadata("C:\\").map(|m| m.permissions()).expect("metadata of C:\\")
    }

    /// Owner, group and mode as Unix reports them. Windows has an owner SID
    /// and an ACL instead; the watched-files code reads those separately
    /// (`crate::win::acl`), so here they are fixed values.
    pub trait MetadataExt {
        fn mode(&self) -> u32;
        fn uid(&self) -> u32;
        fn gid(&self) -> u32;
        fn mtime(&self) -> i64;
        fn blocks(&self) -> u64;
    }

    impl MetadataExt for std::fs::Metadata {
        fn mode(&self) -> u32 {
            let kind = if self.is_dir() {
                0o040000
            } else if self.file_type().is_symlink() {
                0o120000
            } else {
                0o100000
            };
            let perm = if self.is_dir() {
                0o755
            } else if self.permissions().readonly() {
                0o444
            } else {
                0o644
            };
            kind | perm
        }
        fn uid(&self) -> u32 {
            0
        }
        fn gid(&self) -> u32 {
            0
        }
        fn mtime(&self) -> i64 {
            self.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        }
        fn blocks(&self) -> u64 {
            // Allocation size isn't in std's metadata; the length in 512-byte
            // blocks, rounded up, is what `du` would roughly say.
            self.len().div_ceil(512)
        }
    }

    /// Unix creation modes are ignored: new files inherit their folder's ACL.
    pub trait OpenOptionsExt {
        fn mode(&mut self, mode: u32) -> &mut Self;
    }

    impl OpenOptionsExt for std::fs::OpenOptions {
        fn mode(&mut self, _mode: u32) -> &mut Self {
            self
        }
    }

    pub trait DirBuilderExt {
        fn mode(&mut self, mode: u32) -> &mut Self;
    }

    impl DirBuilderExt for std::fs::DirBuilder {
        fn mode(&mut self, _mode: u32) -> &mut Self {
            self
        }
    }

    pub trait FileTypeExt {
        fn is_socket(&self) -> bool;
    }

    impl FileTypeExt for std::fs::FileType {
        fn is_socket(&self) -> bool {
            false
        }
    }

    /// No-op: ownership comes from the folder's ACL on Windows.
    pub fn fchown(_f: &std::fs::File, _uid: Option<u32>, _gid: Option<u32>) -> std::io::Result<()> {
        Ok(())
    }

    pub fn symlink(original: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
        if original.as_ref().is_dir() {
            std::os::windows::fs::symlink_dir(original, link)
        } else {
            std::os::windows::fs::symlink_file(original, link)
        }
    }
}

#[cfg(windows)]
pub mod ffi {
    /// The OS string's bytes. On Windows that is its WTF-8 encoding, which
    /// equals UTF-8 for every path that is valid Unicode.
    pub trait OsStrExt {
        fn as_bytes(&self) -> &[u8];
    }

    impl OsStrExt for std::ffi::OsStr {
        fn as_bytes(&self) -> &[u8] {
            self.as_encoded_bytes()
        }
    }
}

/// Who this process runs as, in the form the server logs callers: the uid
/// on Unix, the account SID on Windows.
pub fn my_id() -> String {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }.to_string()
    }
    #[cfg(windows)]
    {
        crate::win::sys::my_sid()
    }
}

/// True when a caller id (see [`my_id`]) is this process's own account:
/// the node's helpers, link and check runner run as the service account.
pub fn is_self(id: &str) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        id.parse::<u32>().ok() == Some(unsafe { libc::geteuid() })
    }
    #[cfg(windows)]
    {
        static ME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        !id.is_empty() && id == ME.get_or_init(crate::win::sys::my_sid)
    }
}

/// True when this process runs as root (Windows: elevated administrator).
pub fn am_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(windows)]
    {
        crate::win::sys::is_elevated()
    }
}

/// This host's name.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_owned()
    }
    #[cfg(windows)]
    {
        crate::win::sys::hostname()
    }
}

/// (total, used, available) KiB of the filesystem holding `path`, like
/// `df -kP`.
#[cfg(windows)]
pub fn disk_usage(path: &std::path::Path) -> Option<(u64, u64, u64)> {
    crate::metrics::collect::volume_kib(path)
}

/// A local stream to another program's socket: a unix socket, or on
/// Windows a named pipe opened for blocking use. Windows pipes opened this
/// way have no read timeouts; the setters are accepted and do nothing.
#[cfg(unix)]
pub use std::os::unix::net::UnixStream as LocalStream;

#[cfg(windows)]
pub struct LocalStream(std::fs::File);

#[cfg(windows)]
impl LocalStream {
    pub fn connect(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let name = path.as_ref().to_string_lossy().into_owned();
        crate::win::pipe::open_sync(&name).map(Self)
    }
    pub fn set_read_timeout(&self, _t: Option<std::time::Duration>) -> std::io::Result<()> {
        Ok(())
    }
    pub fn set_write_timeout(&self, _t: Option<std::time::Duration>) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
impl std::io::Read for LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.0.read(buf) {
            // A closed pipe is the end of the stream, like a closed socket.
            Err(e) if e.raw_os_error() == Some(109) => Ok(0),
            r => r,
        }
    }
}

#[cfg(windows)]
impl std::io::Write for LocalStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
