//! SQLite connections whose path operations are resolved beneath a pinned directory.
//!
//! SQLite's native VFS remains responsible for I/O, locking, deferred closes, and
//! WAL shared memory.  On Unix we replace its documented system-call entries and
//! dispatch an opaque absolute namespace through `openat`/`fstatat`/`unlinkat`.
//! Non-managed paths are forwarded byte-for-byte to SQLite's original calls.
//!
//! The bundled Unix VFS routes main databases, rollback journals, WAL/SHM,
//! access checks, deletion, symlink expansion, and parent-directory fsync through
//! the hooked `open`, `access`, `stat`, `lstat`, `unlink`, `readlink`, and
//! `openDirectory` entries. The Win32 VFS routes the corresponding operations
//! through `CreateFileW`, `DeleteFileW`, `GetFileAttributesW`, and
//! `GetFullPathNameW`. Data I/O, byte-range locks, file mappings, and deferred
//! closes continue to use SQLite's native VFS unchanged.
//! `/lattice-managed` (and its Win32 spelling) is a reserved internal namespace;
//! an unknown or expired token always fails instead of reaching the host filesystem.
//! Pathname hooks are installed by a platform pre-main constructor because SQLite's
//! syscall table is process-global and may only be changed before application
//! threads start. Executables also call `ManagedSqlite::initialize_process` during
//! single-threaded startup to validate constructor success and detect displacement.

use super::managed_fs::SecureDir;
use rusqlite::{Connection, OpenFlags};
use std::io;

pub struct ManagedSqlite {
    connection: Connection,
    _lease: platform::Lease,
}

impl std::fmt::Debug for ManagedSqlite {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedSqlite")
            .finish_non_exhaustive()
    }
}

impl ManagedSqlite {
    /// Validate that process-startup pathname interposition was installed and
    /// has not subsequently been displaced.
    pub fn initialize_process() -> io::Result<()> {
        platform::initialize_process()
    }
    pub fn open(dir: &SecureDir, leaf: &str, flags: OpenFlags) -> rusqlite::Result<Self> {
        let (path, vfs, lease) = platform::register(dir, leaf)
            .map_err(|e| rusqlite::Error::InvalidPath(std::path::PathBuf::from(e.to_string())))?;
        let connection = Connection::open_with_flags_and_vfs(path, flags, &vfs)?;
        Ok(Self {
            connection,
            _lease: lease,
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
    pub fn sibling_path(&self, leaf: &str) -> rusqlite::Result<std::path::PathBuf> {
        self._lease
            .sibling_path(leaf)
            .map_err(|error| rusqlite::Error::InvalidPath(error.to_string().into()))
    }
}

extern "C" fn initialize_managed_sqlite_before_main() {
    platform::constructor_initialize();
}
#[cfg_attr(target_vendor = "apple", link_section = "__DATA,__mod_init_func")]
#[cfg_attr(all(unix, not(target_vendor = "apple")), link_section = ".init_array")]
#[cfg_attr(windows, link_section = ".CRT$XCU")]
#[used]
static MANAGED_SQLITE_PROCESS_INITIALIZER: extern "C" fn() = initialize_managed_sqlite_before_main;

impl std::ops::Deref for ManagedSqlite {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}
impl std::ops::DerefMut for ManagedSqlite {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.connection
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use rusqlite::ffi;
    use std::collections::HashMap;
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::os::fd::{AsRawFd, IntoRawFd};
    use std::os::raw::{c_char, c_int};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    const PREFIX: &[u8] = b"/lattice-managed/";
    static ROOTS: OnceLock<Mutex<HashMap<String, Arc<File>>>> = OnceLock::new();
    struct VfsContext {
        token: String,
    }
    static VFS_CONTEXTS: OnceLock<Mutex<HashMap<usize, Arc<VfsContext>>>> = OnceLock::new();
    struct TempFileState {
        _name: Vec<u8>,
        _methods: Box<ffi::sqlite3_io_methods>,
        original: *const ffi::sqlite3_io_methods,
    }
    unsafe impl Send for TempFileState {}
    static TEMP_FILES: OnceLock<Mutex<HashMap<usize, TempFileState>>> = OnceLock::new();
    #[cfg(test)]
    pub(super) fn temp_file_count() -> usize {
        TEMP_FILES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }
    static INSTALL: OnceLock<Result<(), String>> = OnceLock::new();
    static STARTUP_INSTALLED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    #[cfg(test)]
    pub(super) static DIRECTORY_SYNCS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    #[cfg(test)]
    pub(super) static TEMP_OPENS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static OPEN: OnceLock<unsafe extern "C" fn(*const c_char, c_int, c_int) -> c_int> =
        OnceLock::new();
    static ACCESS: OnceLock<unsafe extern "C" fn(*const c_char, c_int) -> c_int> = OnceLock::new();
    static STAT: OnceLock<unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int> =
        OnceLock::new();
    static LSTAT: OnceLock<unsafe extern "C" fn(*const c_char, *mut libc::stat) -> c_int> =
        OnceLock::new();
    static UNLINK: OnceLock<unsafe extern "C" fn(*const c_char) -> c_int> = OnceLock::new();
    static OPEN_DIR: OnceLock<unsafe extern "C" fn(*const c_char, *mut c_int) -> c_int> =
        OnceLock::new();
    static READLINK: OnceLock<unsafe extern "C" fn(*const c_char, *mut c_char, usize) -> isize> =
        OnceLock::new();
    type VfsOpen = unsafe extern "C" fn(
        *mut ffi::sqlite3_vfs,
        ffi::sqlite3_filename,
        *mut ffi::sqlite3_file,
        c_int,
        *mut c_int,
    ) -> c_int;
    static NATIVE_VFS_OPEN: OnceLock<VfsOpen> = OnceLock::new();

    pub struct Lease {
        token: String,
        vfs: Box<ffi::sqlite3_vfs>,
        _vfs_name: CString,
    }
    unsafe impl Send for Lease {}
    impl Drop for Lease {
        fn drop(&mut self) {
            unsafe { ffi::sqlite3_vfs_unregister(&mut *self.vfs) };
            VFS_CONTEXTS
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&((&mut *self.vfs) as *mut _ as usize));
            lock_roots().remove(&self.token);
        }
    }
    impl Lease {
        pub fn sibling_path(&self, leaf: &str) -> io::Result<PathBuf> {
            validate_leaf(leaf)?;
            Ok(format!("/lattice-managed/{}/{leaf}", self.token).into())
        }
    }
    fn roots() -> &'static Mutex<HashMap<String, Arc<File>>> {
        ROOTS.get_or_init(|| Mutex::new(HashMap::new()))
    }
    fn lock_roots() -> MutexGuard<'static, HashMap<String, Arc<File>>> {
        roots()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn register(dir: &SecureDir, leaf: &str) -> io::Result<(PathBuf, String, Lease)> {
        validate_leaf(leaf)?;
        require_process_install()?;
        let mut random = [0_u8; 32];
        unsafe { ffi::sqlite3_randomness(random.len() as c_int, random.as_mut_ptr().cast()) };
        let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        lock_roots().insert(token.clone(), Arc::new(dir.try_clone_handle()?));
        let vfs_name_string = format!("lattice-managed-{token}");
        let vfs_name = CString::new(vfs_name_string.clone()).unwrap();
        let mut vfs = unsafe { Box::new(*ffi::sqlite3_vfs_find(std::ptr::null())) };
        let native = (*vfs).xOpen.ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "SQLite native VFS lacks xOpen")
        })?;
        let _ = NATIVE_VFS_OPEN.set(native);
        vfs.zName = vfs_name.as_ptr();
        vfs.xOpen = Some(managed_vfs_open);
        let vfs_key = (&mut *vfs) as *mut _ as usize;
        VFS_CONTEXTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                vfs_key,
                Arc::new(VfsContext {
                    token: token.clone(),
                }),
            );
        let registered = unsafe { ffi::sqlite3_vfs_register(&mut *vfs, 0) };
        if registered != ffi::SQLITE_OK {
            if let Some(contexts) = VFS_CONTEXTS.get() {
                contexts
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&vfs_key);
            }
            lock_roots().remove(&token);
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "could not register managed SQLite VFS",
            ));
        }
        Ok((
            PathBuf::from(format!("/lattice-managed/{token}/{leaf}")),
            vfs_name_string,
            Lease {
                token,
                vfs,
                _vfs_name: vfs_name,
            },
        ))
    }
    unsafe extern "C" fn managed_vfs_open(
        vfs: *mut ffi::sqlite3_vfs,
        name: ffi::sqlite3_filename,
        file: *mut ffi::sqlite3_file,
        flags: c_int,
        out: *mut c_int,
    ) -> c_int {
        let Some(open) = NATIVE_VFS_OPEN.get().copied() else {
            return ffi::SQLITE_IOERR;
        };
        if !name.is_null() {
            return open(vfs, name, file, flags, out);
        }
        #[cfg(test)]
        TEMP_OPENS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let context = match VFS_CONTEXTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
        {
            Ok(map) => map.get(&(vfs as usize)).cloned(),
            Err(p) => p.into_inner().get(&(vfs as usize)).cloned(),
        };
        let Some(context) = context else {
            return ffi::SQLITE_IOERR;
        };
        let mut random = [0u8; 16];
        ffi::sqlite3_randomness(16, random.as_mut_ptr().cast());
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let mut name =
            format!("/lattice-managed/{}/.sqlite-temp-{suffix}", context.token).into_bytes();
        name.extend_from_slice(&[0, 0]);
        let rc = open(vfs, name.as_ptr().cast(), file, flags, out);
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        let original = (*file).pMethods;
        if original.is_null() {
            return ffi::SQLITE_IOERR;
        }
        let mut methods = Box::new(*original);
        methods.xClose = Some(managed_temp_close);
        (*file).pMethods = &*methods;
        TEMP_FILES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                file as usize,
                TempFileState {
                    _name: name,
                    _methods: methods,
                    original,
                },
            );
        ffi::SQLITE_OK
    }
    unsafe extern "C" fn managed_temp_close(file: *mut ffi::sqlite3_file) -> c_int {
        let state = TEMP_FILES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(file as usize));
        let Some(state) = state else {
            return ffi::SQLITE_IOERR_CLOSE;
        };
        (*file).pMethods = state.original;
        let result = match (*state.original).xClose {
            Some(close) => close(file),
            None => ffi::SQLITE_IOERR_CLOSE,
        };
        drop(state);
        result
    }

    fn validate_leaf(s: &str) -> io::Result<()> {
        if s.is_empty()
            || s == "."
            || s == ".."
            || s.as_bytes().contains(&b'/')
            || s.as_bytes().contains(&0)
        {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SQLite name must be one path component",
            ))
        } else {
            Ok(())
        }
    }
    fn decode(p: *const c_char) -> Option<(Arc<File>, CString)> {
        if p.is_null() {
            return None;
        }
        let b = unsafe { CStr::from_ptr(p) }.to_bytes();
        let rest = b.strip_prefix(PREFIX)?;
        let slash = rest.iter().position(|x| *x == b'/')?;
        let token = std::str::from_utf8(&rest[..slash]).ok()?;
        let name = &rest[slash + 1..];
        if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
            set_errno(libc::ENOENT);
            return None;
        }
        let root = lock_roots().get(token).cloned();
        if root.is_none() {
            set_errno(libc::ENOENT);
        }
        Some((root?, CString::new(name).ok()?))
    }
    fn decode_root(p: *const c_char) -> Option<Arc<File>> {
        if p.is_null() {
            return None;
        }
        let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
        let rest = bytes.strip_prefix(PREFIX)?;
        let token = rest.split(|byte| *byte == b'/').next()?;
        let token = std::str::from_utf8(token).ok()?;
        let root = lock_roots().get(token).cloned();
        if root.is_none() {
            set_errno(libc::ENOENT)
        }
        root
    }
    fn managed_prefix(p: *const c_char) -> bool {
        if p.is_null() {
            return false;
        }
        let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
        bytes == b"/lattice-managed" || bytes.starts_with(PREFIX)
    }
    fn missing() -> c_int {
        set_errno(libc::ENOENT);
        -1
    }

    fn install() -> io::Result<()> {
        let result = INSTALL.get_or_init(|| unsafe {
            let v = ffi::sqlite3_vfs_find(std::ptr::null());
            if v.is_null() {
                return Err("SQLite has no default VFS".into());
            }
            let get = (*v)
                .xGetSystemCall
                .ok_or("SQLite VFS lacks xGetSystemCall")?;
            let set = (*v)
                .xSetSystemCall
                .ok_or("SQLite VFS lacks xSetSystemCall")?;
            let names = [
                b"open\0".as_ptr(),
                b"access\0".as_ptr(),
                b"stat\0".as_ptr(),
                b"lstat\0".as_ptr(),
                b"unlink\0".as_ptr(),
                b"openDirectory\0".as_ptr(),
                b"readlink\0".as_ptr(),
            ];
            let replacements = [
                managed_open as usize,
                managed_access as usize,
                managed_stat as usize,
                managed_lstat as usize,
                managed_unlink as usize,
                managed_open_dir as usize,
                managed_readlink as usize,
            ];
            let originals: Vec<_> = names.iter().map(|n| get(v, (*n).cast())).collect();
            if originals.iter().any(Option::is_none) {
                return Err("SQLite VFS lacks a required pathname syscall".into());
            }
            let _ = OPEN.set(std::mem::transmute(originals[0].unwrap()));
            let _ = ACCESS.set(std::mem::transmute(originals[1].unwrap()));
            let _ = STAT.set(std::mem::transmute(originals[2].unwrap()));
            let _ = LSTAT.set(std::mem::transmute(originals[3].unwrap()));
            let _ = UNLINK.set(std::mem::transmute(originals[4].unwrap()));
            let _ = OPEN_DIR.set(std::mem::transmute(originals[5].unwrap()));
            let _ = READLINK.set(std::mem::transmute(originals[6].unwrap()));
            for index in 0..names.len() {
                let replacement: unsafe extern "C" fn() = std::mem::transmute(replacements[index]);
                if set(v, names[index].cast(), Some(replacement)) != ffi::SQLITE_OK {
                    for rollback in 0..index {
                        let _ = set(v, names[rollback].cast(), originals[rollback]);
                    }
                    return Err("could not atomically install SQLite pathname syscalls".into());
                }
            }
            Ok(())
        });
        result
            .clone()
            .map_err(|detail| io::Error::new(io::ErrorKind::Unsupported, detail))
    }
    pub fn constructor_initialize() {
        if install().is_ok() {
            STARTUP_INSTALLED.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    pub fn initialize_process() -> io::Result<()> {
        require_process_install()
    }
    fn require_process_install() -> io::Result<()> {
        if !STARTUP_INSTALLED.load(std::sync::atomic::Ordering::Acquire) {
            if let Some(Err(detail)) = INSTALL.get() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("managed SQLite pre-main installation failed: {detail}"),
                ));
            }
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "managed SQLite process hooks were not installed before runtime startup",
            ));
        }
        verify_hooks()
    }
    #[cfg(test)]
    pub(super) fn startup_installed() -> bool {
        STARTUP_INSTALLED.load(std::sync::atomic::Ordering::Acquire)
    }
    fn verify_hooks() -> io::Result<()> {
        unsafe {
            let v = ffi::sqlite3_vfs_find(std::ptr::null());
            let get = if v.is_null() {
                None
            } else {
                (*v).xGetSystemCall
            }
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "SQLite syscall hooks unavailable",
                )
            })?;
            for (name, expected) in [
                (b"open\0".as_ptr(), managed_open as usize),
                (b"access\0".as_ptr(), managed_access as usize),
                (b"stat\0".as_ptr(), managed_stat as usize),
                (b"lstat\0".as_ptr(), managed_lstat as usize),
                (b"unlink\0".as_ptr(), managed_unlink as usize),
                (b"openDirectory\0".as_ptr(), managed_open_dir as usize),
                (b"readlink\0".as_ptr(), managed_readlink as usize),
            ] {
                let actual = get(v, name.cast()).map(|f| f as usize);
                if actual != Some(expected) {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "SQLite managed syscall hook was displaced",
                    ));
                }
            }
        }
        Ok(())
    }

    unsafe extern "C" fn managed_open(p: *const c_char, f: c_int, m: c_int) -> c_int {
        if let Some((d, n)) = decode(p) {
            let fd = libc::openat(
                d.as_raw_fd(),
                n.as_ptr(),
                f | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                m,
            );
            if fd < 0
                && current_errno() == libc::ENOENT
                && f & libc::O_CREAT != 0
                && f & libc::O_EXCL == 0
            {
                libc::openat(
                    d.as_raw_fd(),
                    n.as_ptr(),
                    (f & !libc::O_CREAT) | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    m,
                )
            } else {
                fd
            }
        } else if managed_prefix(p) {
            missing()
        } else {
            OPEN.get()
                .copied()
                .map(|call| call(p, f, m))
                .unwrap_or_else(|| {
                    set_errno(libc::EIO);
                    -1
                })
        }
    }
    unsafe extern "C" fn managed_access(p: *const c_char, m: c_int) -> c_int {
        if let Some((d, n)) = decode(p) {
            libc::faccessat(
                d.as_raw_fd(),
                n.as_ptr(),
                m,
                libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW,
            )
        } else if managed_prefix(p) {
            missing()
        } else {
            ACCESS
                .get()
                .copied()
                .map(|call| call(p, m))
                .unwrap_or_else(|| {
                    set_errno(libc::EIO);
                    -1
                })
        }
    }
    unsafe extern "C" fn managed_stat(p: *const c_char, s: *mut libc::stat) -> c_int {
        if let Some((d, n)) = decode(p) {
            libc::fstatat(d.as_raw_fd(), n.as_ptr(), s, libc::AT_SYMLINK_NOFOLLOW)
        } else if managed_prefix(p) {
            missing()
        } else {
            STAT.get()
                .copied()
                .map(|call| call(p, s))
                .unwrap_or_else(|| {
                    set_errno(libc::EIO);
                    -1
                })
        }
    }
    unsafe extern "C" fn managed_lstat(p: *const c_char, s: *mut libc::stat) -> c_int {
        if managed_prefix(p) {
            managed_stat(p, s)
        } else {
            LSTAT
                .get()
                .copied()
                .map(|call| call(p, s))
                .unwrap_or_else(|| {
                    set_errno(libc::EIO);
                    -1
                })
        }
    }
    unsafe extern "C" fn managed_unlink(p: *const c_char) -> c_int {
        if let Some((d, n)) = decode(p) {
            libc::unlinkat(d.as_raw_fd(), n.as_ptr(), 0)
        } else if managed_prefix(p) {
            missing()
        } else {
            UNLINK
                .get()
                .copied()
                .map(|call| call(p))
                .unwrap_or_else(|| {
                    set_errno(libc::EIO);
                    -1
                })
        }
    }
    unsafe extern "C" fn managed_open_dir(p: *const c_char, out: *mut c_int) -> c_int {
        if let Some(d) = decode_root(p) {
            #[cfg(test)]
            DIRECTORY_SYNCS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match d.try_clone() {
                Ok(f) => {
                    *out = f.into_raw_fd();
                    0
                }
                Err(e) => {
                    set_errno(e.raw_os_error().unwrap_or(libc::EIO));
                    -1
                }
            }
        } else if managed_prefix(p) {
            missing()
        } else {
            OPEN_DIR
                .get()
                .copied()
                .map(|call| call(p, out))
                .unwrap_or_else(|| {
                    set_errno(libc::EIO);
                    -1
                })
        }
    }
    unsafe extern "C" fn managed_readlink(p: *const c_char, b: *mut c_char, n: usize) -> isize {
        if managed_prefix(p) {
            set_errno(libc::EINVAL);
            -1
        } else {
            READLINK
                .get()
                .copied()
                .map(|call| call(p, b, n))
                .unwrap_or_else(|| {
                    set_errno(libc::EIO);
                    -1
                })
        }
    }
    #[cfg(target_os = "macos")]
    fn current_errno() -> c_int {
        unsafe { *libc::__error() }
    }
    #[cfg(not(target_os = "macos"))]
    fn current_errno() -> c_int {
        unsafe { *libc::__errno_location() }
    }
    #[cfg(target_os = "macos")]
    fn set_errno(e: c_int) {
        unsafe { *libc::__error() = e }
    }
    #[cfg(not(target_os = "macos"))]
    fn set_errno(e: c_int) {
        unsafe { *libc::__errno_location() = e }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::fs;

    fn rw() -> OpenFlags {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    }

    #[test]
    fn renamed_root_remains_the_database_authority_with_wal() {
        let parent = tempfile::tempdir().unwrap();
        let live = parent.path().join("live");
        let pinned = parent.path().join("pinned");
        let replacement = parent.path().join("replacement");
        fs::create_dir(&live).unwrap();
        let authority = SecureDir::open(&live).unwrap();
        fs::rename(&live, &pinned).unwrap();
        fs::create_dir(&live).unwrap();
        let db = ManagedSqlite::open(&authority, "state.db", rw()).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        db.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(1)")
            .unwrap();
        assert!(pinned.join("state.db").is_file());
        assert_eq!(fs::read_dir(&live).unwrap().count(), 0);
        fs::rename(&live, &replacement).unwrap();
        db.execute("INSERT INTO t VALUES(2)", []).unwrap();
    }

    #[test]
    fn native_and_managed_windows_connections_share_stock_locks() {
        let root = tempfile::tempdir().unwrap();
        let authority = SecureDir::open(root.path()).unwrap();
        let managed = ManagedSqlite::open(&authority, "shared.db", rw()).unwrap();
        managed.pragma_update(None, "journal_mode", "WAL").unwrap();
        managed
            .execute_batch("CREATE TABLE t(v INTEGER); INSERT INTO t VALUES(0)")
            .unwrap();
        let native = Connection::open(root.path().join("shared.db")).unwrap();
        for _ in 0..20 {
            managed.execute("UPDATE t SET v=v+1", []).unwrap();
            native.execute("UPDATE t SET v=v+1", []).unwrap();
        }
        assert_eq!(
            native
                .query_row("SELECT v FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            40
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn rw() -> OpenFlags {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    }

    #[test]
    fn process_hooks_are_installed_before_libtest_and_native_sqlite_use() {
        assert!(platform::startup_installed());
        let native = Connection::open_in_memory().unwrap();
        native
            .query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
            .unwrap();
        ManagedSqlite::initialize_process().unwrap();
    }

    #[test]
    fn directory_replacement_cannot_redirect_database_or_wal_sidecars() {
        let parent = tempfile::tempdir().unwrap();
        let live = parent.path().join("live");
        let pinned = parent.path().join("pinned");
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(&live).unwrap();
        let authority = SecureDir::open(&live).unwrap();
        fs::rename(&live, &pinned).unwrap();
        symlink(outside.path(), &live).unwrap();

        let db = ManagedSqlite::open(&authority, "state.db", rw()).unwrap();
        assert_eq!(db.pragma_update(None, "journal_mode", "WAL").unwrap(), ());
        db.execute_batch("CREATE TABLE values_(value INTEGER); INSERT INTO values_ VALUES(7);")
            .unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM values_", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert!(pinned.join("state.db").is_file());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn native_and_managed_connections_share_wal_locks_and_deferred_close_state() {
        let root = tempfile::tempdir().unwrap();
        let authority = SecureDir::open(root.path()).unwrap();
        let managed = ManagedSqlite::open(&authority, "shared.db", rw()).unwrap();
        managed.pragma_update(None, "journal_mode", "WAL").unwrap();
        managed
            .execute_batch("CREATE TABLE counter(value INTEGER); INSERT INTO counter VALUES(0);")
            .unwrap();
        let native = Connection::open(root.path().join("shared.db")).unwrap();
        native
            .busy_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        managed
            .busy_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        for _ in 0..20 {
            managed
                .execute("UPDATE counter SET value=value+1", [])
                .unwrap();
            native
                .execute("UPDATE counter SET value=value+1", [])
                .unwrap();
        }
        assert_eq!(
            managed
                .query_row("SELECT value FROM counter", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            40
        );
        drop(native);
        managed
            .execute("UPDATE counter SET value=value+1", [])
            .unwrap();
        assert_eq!(
            managed
                .query_row("SELECT value FROM counter", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            41
        );
    }

    #[test]
    fn malformed_or_unknown_managed_names_never_fall_back_to_the_os_namespace() {
        let root = tempfile::tempdir().unwrap();
        let authority = SecureDir::open(root.path()).unwrap();
        let installed = ManagedSqlite::open(&authority, "installed.db", rw()).unwrap();
        let error =
            Connection::open_with_flags("/lattice-managed/unknown/state.db", rw()).unwrap_err();
        assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));
        drop(installed);
    }

    #[test]
    fn twenty_managed_authorities_share_wal_and_checkpoint_state() {
        let root = tempfile::tempdir().unwrap();
        let authority = SecureDir::open(root.path()).unwrap();
        let setup = ManagedSqlite::open(&authority, "owners.db", rw()).unwrap();
        setup.pragma_update(None, "journal_mode", "WAL").unwrap();
        setup
            .execute_batch("CREATE TABLE counter(v INTEGER); INSERT INTO counter VALUES(0)")
            .unwrap();
        drop(setup);
        let root = std::sync::Arc::new(root);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(20));
        let threads: Vec<_> = (0..20)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let dir = SecureDir::open(root.path()).unwrap();
                    let db = ManagedSqlite::open(&dir, "owners.db", rw()).unwrap();
                    db.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
                    barrier.wait();
                    db.execute("UPDATE counter SET v=v+1", []).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let db =
            ManagedSqlite::open(&SecureDir::open(root.path()).unwrap(), "owners.db", rw()).unwrap();
        assert_eq!(
            db.query_row("SELECT v FROM counter", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            20
        );
        let checkpoint: (i64, i64, i64) = db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(checkpoint.0, 0);
    }

    #[test]
    fn durable_commit_opens_the_pinned_parent_for_directory_sync() {
        platform::DIRECTORY_SYNCS.store(0, std::sync::atomic::Ordering::SeqCst);
        let root = tempfile::tempdir().unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        let db = ManagedSqlite::open(&dir, "sync.db", rw()).unwrap();
        db.pragma_update(None, "synchronous", "FULL").unwrap();
        db.execute_batch("CREATE TABLE durable(v); INSERT INTO durable VALUES(1)")
            .unwrap();
        assert!(platform::DIRECTORY_SYNCS.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    #[test]
    fn null_named_sqlite_temp_files_use_the_connection_authority() {
        platform::TEMP_OPENS.store(0, std::sync::atomic::Ordering::SeqCst);
        let root = tempfile::tempdir().unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        let db = ManagedSqlite::open(&dir, "temp.db", rw()).unwrap();
        db.pragma_update(None, "temp_store", "FILE").unwrap();
        db.execute_batch("CREATE TEMP TABLE spill(v BLOB)").unwrap();
        let payload = vec![7u8; 128 * 1024];
        for _ in 0..32 {
            db.execute("INSERT INTO spill VALUES(?1)", [&payload])
                .unwrap();
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM spill", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            32
        );
        assert!(platform::TEMP_OPENS.load(std::sync::atomic::Ordering::SeqCst) > 0);
        drop(db);
        assert_eq!(platform::temp_file_count(), 0);
    }

    #[test]
    fn crash_reopen_recovers_managed_wal() {
        const ENV: &str = "LATTICE_MANAGED_SQLITE_CRASH_CHILD";
        if let Some(path) = std::env::var_os(ENV) {
            let dir = SecureDir::open(std::path::Path::new(&path)).unwrap();
            let db = ManagedSqlite::open(&dir, "crash.db", rw()).unwrap();
            db.pragma_update(None, "journal_mode", "WAL").unwrap();
            db.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
            db.execute_batch("CREATE TABLE IF NOT EXISTS crash(v); INSERT INTO crash VALUES(9)")
                .unwrap();
            std::process::exit(0);
        }
        let root = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("storage::managed_sqlite::tests::crash_reopen_recovers_managed_wal")
            .arg("--nocapture")
            .env(ENV, root.path())
            .status()
            .unwrap();
        assert!(status.success());
        let db =
            ManagedSqlite::open(&SecureDir::open(root.path()).unwrap(), "crash.db", rw()).unwrap();
        assert_eq!(
            db.query_row("SELECT v FROM crash", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            9
        );
        let checkpoint: (i64, i64, i64) = db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(checkpoint.0, 0);
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use rusqlite::ffi;
    use std::collections::HashMap;
    use std::ffi::{c_void, CString};
    use std::fs::File;
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::PathBuf;
    use std::ptr;
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use windows_sys::Win32::Foundation::{SetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, GetFileInformationByHandle, SetFileInformationByHandle,
        BY_HANDLE_FILE_INFORMATION, FILE_DISPOSITION_INFO,
    };

    const PREFIX: &[u16] = &[
        'C' as u16,
        ':' as u16,
        '\\' as u16,
        'l' as u16,
        'a' as u16,
        't' as u16,
        't' as u16,
        'i' as u16,
        'c' as u16,
        'e' as u16,
        '-' as u16,
        'm' as u16,
        'a' as u16,
        'n' as u16,
        'a' as u16,
        'g' as u16,
        'e' as u16,
        'd' as u16,
        '\\' as u16,
    ];
    const ERROR_PATH_NOT_FOUND: u32 = 3;
    const ERROR_INVALID_FUNCTION: u32 = 1;
    const FILE_OPEN: u32 = 1;
    const FILE_CREATE: u32 = 2;
    const FILE_OPEN_IF: u32 = 3;
    const FILE_OVERWRITE: u32 = 4;
    const FILE_OVERWRITE_IF: u32 = 5;
    const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
    const FILE_OPEN_REPARSE_POINT: u32 = 0x20_0000;
    const FILE_DELETE_ON_CLOSE: u32 = 0x1000;
    const DELETE_ACCESS: u32 = 0x1_0000;
    const READ_ATTRIBUTES: u32 = 0x80;

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }
    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root: HANDLE,
        name: *mut UnicodeString,
        attributes: u32,
        security: *mut c_void,
        qos: *mut c_void,
    }
    #[repr(C)]
    union IoValue {
        status: i32,
        pointer: *mut c_void,
    }
    #[repr(C)]
    struct IoStatus {
        value: IoValue,
        information: usize,
    }
    #[link(name = "ntdll")]
    extern "system" {
        fn NtCreateFile(
            handle: *mut HANDLE,
            access: u32,
            attrs: *mut ObjectAttributes,
            status: *mut IoStatus,
            allocation: *mut i64,
            file_attrs: u32,
            share: u32,
            disposition: u32,
            options: u32,
            ea: *mut c_void,
            ea_len: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    type CreateFileW =
        unsafe extern "system" fn(*const u16, u32, u32, *mut c_void, u32, u32, HANDLE) -> HANDLE;
    type DeleteFileW = unsafe extern "system" fn(*const u16) -> i32;
    type GetAttributesW = unsafe extern "system" fn(*const u16) -> u32;
    type GetFullPathW = unsafe extern "system" fn(*const u16, u32, *mut u16, *mut *mut u16) -> u32;
    static ROOTS: OnceLock<Mutex<HashMap<String, Arc<File>>>> = OnceLock::new();
    struct VfsContext {
        token: String,
    }
    static VFS_CONTEXTS: OnceLock<Mutex<HashMap<usize, Arc<VfsContext>>>> = OnceLock::new();
    struct TempFileState {
        _name: Vec<u8>,
        _methods: Box<ffi::sqlite3_io_methods>,
        original: *const ffi::sqlite3_io_methods,
    }
    unsafe impl Send for TempFileState {}
    static TEMP_FILES: OnceLock<Mutex<HashMap<usize, TempFileState>>> = OnceLock::new();
    static INSTALL: OnceLock<Result<(), String>> = OnceLock::new();
    static STARTUP_INSTALLED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    static CREATE: OnceLock<CreateFileW> = OnceLock::new();
    static DELETE: OnceLock<DeleteFileW> = OnceLock::new();
    static ATTRS: OnceLock<GetAttributesW> = OnceLock::new();
    static FULL: OnceLock<GetFullPathW> = OnceLock::new();
    type VfsOpen = unsafe extern "C" fn(
        *mut ffi::sqlite3_vfs,
        ffi::sqlite3_filename,
        *mut ffi::sqlite3_file,
        i32,
        *mut i32,
    ) -> i32;
    static NATIVE_VFS_OPEN: OnceLock<VfsOpen> = OnceLock::new();

    pub struct Lease {
        token: String,
        vfs: Box<ffi::sqlite3_vfs>,
        _vfs_name: CString,
    }
    unsafe impl Send for Lease {}
    impl Drop for Lease {
        fn drop(&mut self) {
            unsafe { ffi::sqlite3_vfs_unregister(&mut *self.vfs) };
            VFS_CONTEXTS
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&((&mut *self.vfs) as *mut _ as usize));
            lock_roots().remove(&self.token);
        }
    }
    impl Lease {
        pub fn sibling_path(&self, leaf: &str) -> io::Result<PathBuf> {
            validate_leaf(leaf)?;
            Ok(format!(r"C:\lattice-managed\{}\{leaf}", self.token).into())
        }
    }
    fn lock_roots() -> MutexGuard<'static, HashMap<String, Arc<File>>> {
        ROOTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }
    fn validate_leaf(leaf: &str) -> io::Result<()> {
        if leaf.is_empty()
            || leaf == "."
            || leaf == ".."
            || leaf.chars().any(|c| c == '/' || c == '\\' || c == '\0')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SQLite name must be one path component",
            ));
        }
        Ok(())
    }
    pub fn register(dir: &SecureDir, leaf: &str) -> io::Result<(PathBuf, String, Lease)> {
        validate_leaf(leaf)?;
        require_process_install()?;
        let mut random = [0u8; 32];
        unsafe { ffi::sqlite3_randomness(32, random.as_mut_ptr().cast()) };
        let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
        lock_roots().insert(token.clone(), Arc::new(dir.try_clone_handle()?));
        let vfs_name_string = format!("lattice-managed-{token}");
        let vfs_name = CString::new(vfs_name_string.clone()).unwrap();
        let mut vfs = unsafe { Box::new(*ffi::sqlite3_vfs_find(ptr::null())) };
        let native = (*vfs).xOpen.ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "SQLite native VFS lacks xOpen")
        })?;
        let _ = NATIVE_VFS_OPEN.set(native);
        vfs.zName = vfs_name.as_ptr();
        vfs.xOpen = Some(managed_vfs_open);
        let vfs_key = (&mut *vfs) as *mut _ as usize;
        VFS_CONTEXTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                vfs_key,
                Arc::new(VfsContext {
                    token: token.clone(),
                }),
            );
        if unsafe { ffi::sqlite3_vfs_register(&mut *vfs, 0) } != ffi::SQLITE_OK {
            if let Some(contexts) = VFS_CONTEXTS.get() {
                contexts
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&vfs_key);
            }
            lock_roots().remove(&token);
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "could not register managed SQLite VFS",
            ));
        }
        Ok((
            PathBuf::from(format!(r"C:\lattice-managed\{token}\{leaf}")),
            vfs_name_string,
            Lease {
                token,
                vfs,
                _vfs_name: vfs_name,
            },
        ))
    }
    unsafe extern "C" fn managed_vfs_open(
        vfs: *mut ffi::sqlite3_vfs,
        name: ffi::sqlite3_filename,
        file: *mut ffi::sqlite3_file,
        flags: i32,
        out: *mut i32,
    ) -> i32 {
        let Some(open) = NATIVE_VFS_OPEN.get().copied() else {
            return ffi::SQLITE_IOERR;
        };
        if !name.is_null() {
            return open(vfs, name, file, flags, out);
        }
        let context = match VFS_CONTEXTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
        {
            Ok(m) => m.get(&(vfs as usize)).cloned(),
            Err(p) => p.into_inner().get(&(vfs as usize)).cloned(),
        };
        let Some(context) = context else {
            return ffi::SQLITE_IOERR;
        };
        let mut random = [0u8; 16];
        ffi::sqlite3_randomness(16, random.as_mut_ptr().cast());
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let mut name = format!(
            r"C:\lattice-managed\{}\.sqlite-temp-{suffix}",
            context.token
        )
        .into_bytes();
        name.extend_from_slice(&[0, 0]);
        let rc = open(vfs, name.as_ptr().cast(), file, flags, out);
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        let original = (*file).pMethods;
        if original.is_null() {
            return ffi::SQLITE_IOERR;
        }
        let mut methods = Box::new(*original);
        methods.xClose = Some(managed_temp_close);
        (*file).pMethods = &*methods;
        TEMP_FILES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                file as usize,
                TempFileState {
                    _name: name,
                    _methods: methods,
                    original,
                },
            );
        ffi::SQLITE_OK
    }
    unsafe extern "C" fn managed_temp_close(file: *mut ffi::sqlite3_file) -> i32 {
        let state = TEMP_FILES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(file as usize));
        let Some(state) = state else {
            return ffi::SQLITE_IOERR_CLOSE;
        };
        (*file).pMethods = state.original;
        let result = match (*state.original).xClose {
            Some(close) => close(file),
            None => ffi::SQLITE_IOERR_CLOSE,
        };
        drop(state);
        result
    }
    fn wide(p: *const u16) -> Option<Vec<u16>> {
        if p.is_null() {
            return None;
        }
        let mut n = 0;
        unsafe {
            while *p.add(n) != 0 {
                n += 1
            }
            Some(std::slice::from_raw_parts(p, n).to_vec())
        }
    }
    fn prefix_matches(v: &[u16]) -> bool {
        v.len() >= PREFIX.len()
            && v[..PREFIX.len()]
                .iter()
                .zip(PREFIX)
                .all(|(a, b)| (*a as u8).eq_ignore_ascii_case(&(*b as u8)))
    }
    fn is_prefix(p: *const u16) -> bool {
        wide(p).is_some_and(|v| {
            prefix_matches(&v)
                || (v.len() + 1 == PREFIX.len()
                    && v.iter()
                        .zip(&PREFIX[..v.len()])
                        .all(|(a, b)| (*a as u8).eq_ignore_ascii_case(&(*b as u8))))
        })
    }
    fn decode(p: *const u16) -> Option<(Arc<File>, Vec<u16>)> {
        let v = wide(p)?;
        if !prefix_matches(&v) {
            return None;
        }
        let rest = &v[PREFIX.len()..];
        let slash = rest.iter().position(|c| *c == b'\\' as u16)?;
        let token = String::from_utf16(&rest[..slash]).ok()?;
        let name = &rest[slash + 1..];
        if name.is_empty() || name.iter().any(|c| *c == b'/' as u16 || *c == b'\\' as u16) {
            unsafe { SetLastError(ERROR_PATH_NOT_FOUND) };
            return None;
        }
        let root = lock_roots().get(&token).cloned();
        if root.is_none() {
            unsafe { SetLastError(ERROR_PATH_NOT_FOUND) }
        }
        Some((root?, name.to_vec()))
    }
    unsafe fn nt_open(
        root: &File,
        name: &[u16],
        access: u32,
        share: u32,
        disp: u32,
        attrs: u32,
        flags: u32,
    ) -> HANDLE {
        let mut name = name.to_vec();
        let bytes = (name.len() * 2) as u16;
        let mut unicode = UnicodeString {
            length: bytes,
            maximum_length: bytes,
            buffer: name.as_mut_ptr(),
        };
        let mut oa = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root: root.as_raw_handle() as HANDLE,
            name: &mut unicode,
            attributes: 0x40,
            security: ptr::null_mut(),
            qos: ptr::null_mut(),
        };
        let mut io: IoStatus = zeroed();
        let mut handle = INVALID_HANDLE_VALUE;
        let options = FILE_NON_DIRECTORY_FILE
            | FILE_OPEN_REPARSE_POINT
            | if flags & 0x0400_0000 != 0 {
                FILE_DELETE_ON_CLOSE
            } else {
                0
            };
        let status = NtCreateFile(
            &mut handle,
            access,
            &mut oa,
            &mut io,
            ptr::null_mut(),
            attrs,
            share,
            disp,
            options,
            ptr::null_mut(),
            0,
        );
        if status < 0 {
            SetLastError(RtlNtStatusToDosError(status));
            INVALID_HANDLE_VALUE
        } else {
            handle
        }
    }
    unsafe extern "system" fn managed_create(
        p: *const u16,
        a: u32,
        s: u32,
        sa: *mut c_void,
        d: u32,
        attrs: u32,
        t: HANDLE,
    ) -> HANDLE {
        if let Some((root, name)) = decode(p) {
            let disp = match d {
                1 => FILE_CREATE,
                2 => FILE_OVERWRITE_IF,
                3 => FILE_OPEN,
                4 => FILE_OPEN_IF,
                5 => FILE_OVERWRITE,
                _ => {
                    SetLastError(ERROR_INVALID_FUNCTION);
                    return INVALID_HANDLE_VALUE;
                }
            };
            nt_open(&root, &name, a, s, disp, attrs, attrs)
        } else if is_prefix(p) {
            SetLastError(ERROR_PATH_NOT_FOUND);
            INVALID_HANDLE_VALUE
        } else {
            CREATE
                .get()
                .copied()
                .map(|f| f(p, a, s, sa, d, attrs, t))
                .unwrap_or_else(|| {
                    SetLastError(ERROR_INVALID_FUNCTION);
                    INVALID_HANDLE_VALUE
                })
        }
    }
    unsafe extern "system" fn managed_delete(p: *const u16) -> i32 {
        if let Some((root, name)) = decode(p) {
            let h = nt_open(
                &root,
                &name,
                DELETE_ACCESS | READ_ATTRIBUTES,
                7,
                FILE_OPEN,
                0,
                0,
            );
            if h == INVALID_HANDLE_VALUE {
                return 0;
            }
            let file = File::from_raw_handle(h as _);
            let info = FILE_DISPOSITION_INFO { DeleteFile: 1 };
            let ok = SetFileInformationByHandle(
                file.as_raw_handle() as HANDLE,
                FileDispositionInfo,
                &info as *const _ as _,
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            );
            drop(file);
            ok
        } else if is_prefix(p) {
            SetLastError(ERROR_PATH_NOT_FOUND);
            0
        } else {
            DELETE.get().copied().map(|f| f(p)).unwrap_or_else(|| {
                SetLastError(ERROR_INVALID_FUNCTION);
                0
            })
        }
    }
    unsafe extern "system" fn managed_attrs(p: *const u16) -> u32 {
        if let Some((root, name)) = decode(p) {
            let h = nt_open(&root, &name, READ_ATTRIBUTES, 7, FILE_OPEN, 0, 0);
            if h == INVALID_HANDLE_VALUE {
                return u32::MAX;
            }
            let file = File::from_raw_handle(h as _);
            let mut info: BY_HANDLE_FILE_INFORMATION = zeroed();
            if GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) == 0 {
                u32::MAX
            } else {
                info.dwFileAttributes
            }
        } else if is_prefix(p) {
            SetLastError(ERROR_PATH_NOT_FOUND);
            u32::MAX
        } else {
            ATTRS.get().copied().map(|f| f(p)).unwrap_or_else(|| {
                SetLastError(ERROR_INVALID_FUNCTION);
                u32::MAX
            })
        }
    }
    unsafe extern "system" fn managed_full(
        p: *const u16,
        n: u32,
        out: *mut u16,
        part: *mut *mut u16,
    ) -> u32 {
        if is_prefix(p) {
            let v = wide(p).unwrap_or_default();
            let required = v.len() as u32 + 1;
            if n == 0 {
                return required;
            }
            if n < required {
                return required;
            }
            ptr::copy_nonoverlapping(v.as_ptr(), out, v.len());
            *out.add(v.len()) = 0;
            if !part.is_null() {
                *part = ptr::null_mut()
            }
            v.len() as u32
        } else {
            FULL.get()
                .copied()
                .map(|f| f(p, n, out, part))
                .unwrap_or_else(|| {
                    SetLastError(ERROR_INVALID_FUNCTION);
                    0
                })
        }
    }
    fn install() -> io::Result<()> {
        let result = INSTALL.get_or_init(|| unsafe {
            let v = ffi::sqlite3_vfs_find(ptr::null());
            if v.is_null() {
                return Err("SQLite has no default VFS".into());
            }
            let get = (*v)
                .xGetSystemCall
                .ok_or("SQLite VFS lacks xGetSystemCall")?;
            let set = (*v)
                .xSetSystemCall
                .ok_or("SQLite VFS lacks xSetSystemCall")?;
            let names = [
                b"CreateFileW\0".as_ptr(),
                b"DeleteFileW\0".as_ptr(),
                b"GetFileAttributesW\0".as_ptr(),
                b"GetFullPathNameW\0".as_ptr(),
            ];
            let replacements = [
                managed_create as *const () as usize,
                managed_delete as *const () as usize,
                managed_attrs as *const () as usize,
                managed_full as *const () as usize,
            ];
            let originals: Vec<_> = names.iter().map(|name| get(v, (*name).cast())).collect();
            if originals.iter().any(Option::is_none) {
                return Err("SQLite Win32 VFS lacks a required pathname syscall".into());
            }
            let _ = CREATE.set(std::mem::transmute(originals[0].unwrap()));
            let _ = DELETE.set(std::mem::transmute(originals[1].unwrap()));
            let _ = ATTRS.set(std::mem::transmute(originals[2].unwrap()));
            let _ = FULL.set(std::mem::transmute(originals[3].unwrap()));
            for index in 0..names.len() {
                let replacement: unsafe extern "C" fn() = std::mem::transmute(replacements[index]);
                if set(v, names[index].cast(), Some(replacement)) != ffi::SQLITE_OK {
                    for rollback in 0..index {
                        let _ = set(v, names[rollback].cast(), originals[rollback]);
                    }
                    return Err(
                        "could not atomically install SQLite Win32 pathname syscalls".into(),
                    );
                }
            }
            Ok(())
        });
        result
            .clone()
            .map_err(|e| io::Error::new(io::ErrorKind::Unsupported, e))
    }
    pub fn constructor_initialize() {
        if install().is_ok() {
            STARTUP_INSTALLED.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    pub fn initialize_process() -> io::Result<()> {
        require_process_install()
    }
    fn require_process_install() -> io::Result<()> {
        if !STARTUP_INSTALLED.load(std::sync::atomic::Ordering::Acquire) {
            if let Some(Err(detail)) = INSTALL.get() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("managed SQLite pre-main installation failed: {detail}"),
                ));
            }
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "managed SQLite process hooks were not installed before runtime startup",
            ));
        }
        verify_hooks()
    }
    fn verify_hooks() -> io::Result<()> {
        unsafe {
            let v = ffi::sqlite3_vfs_find(ptr::null());
            let get = if v.is_null() {
                None
            } else {
                (*v).xGetSystemCall
            }
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "SQLite syscall hooks unavailable",
                )
            })?;
            for (name, expected) in [
                (b"CreateFileW\0".as_ptr(), managed_create as usize),
                (b"DeleteFileW\0".as_ptr(), managed_delete as usize),
                (b"GetFileAttributesW\0".as_ptr(), managed_attrs as usize),
                (b"GetFullPathNameW\0".as_ptr(), managed_full as usize),
            ] {
                if get(v, name.cast()).map(|f| f as usize) != Some(expected) {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "SQLite managed syscall hook was displaced",
                    ));
                }
            }
            Ok(())
        }
    }
}
