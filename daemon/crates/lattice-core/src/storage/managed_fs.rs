//! Descriptor-relative access to repository-managed storage.
//!
//! `SecureDir::open` accepts only a trusted, already-authorized root path; its
//! intermediate components may be resolved by the operating system. A
//! `SecureDir` then pins the opened directory inode. All interior traversal uses
//! `openat(2)` with `O_NOFOLLOW`; untrusted interior paths are never
//! canonicalized or resolved again through the process working directory.
use serde::{Deserialize, Serialize};

/// Identity and mutation stamp for a pinned directory.  Callers persist this
/// beside an opaque continuation and must restart an inventory when it changes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedDirFingerprint {
    pub device: u64,
    pub inode: u64,
    pub modified_seconds: i64,
    pub modified_nanos: i64,
}

/// Opaque, reopen-safe directory continuation on Unix. Windows continuations
/// are deliberately tied to the pinned query handle and reject after reopen.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ManagedDirCursor {
    Unix(i64),
    MacOs {
        kernel_block_start: i64,
        consumed_in_block: u32,
    },
    Windows(u64),
}

#[cfg(unix)]
mod platform {
    use super::{ManagedDirCursor, ManagedDirFingerprint};
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path, PathBuf};
    #[cfg(test)]
    thread_local! { static BEFORE_MUTATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }
    #[cfg(test)]
    thread_local! { static BEFORE_DIRECTORY_METADATA: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Identity {
        pub dev: u64,
        pub ino: u64,
    }
    #[derive(Debug, Clone)]
    pub struct Entry {
        pub name: String,
        pub identity: Identity,
        pub len: u64,
        pub allocated: u64,
        pub is_file: bool,
        pub is_dir: bool,
    }
    #[derive(Debug)]
    pub struct DirPage {
        pub entries: Vec<Entry>,
        pub next_cookie: Option<ManagedDirCursor>,
    }
    #[derive(Debug)]
    pub struct SecureDir {
        file: File,
        display: PathBuf,
    }

    impl SecureDir {
        pub fn open(path: &Path) -> io::Result<Self> {
            let c = cstring(path.as_os_str().as_bytes())?;
            let fd = unsafe {
                libc::open(
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let display = path.canonicalize()?;
            Ok(Self { file, display })
        }

        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                file: self.file.try_clone()?,
                display: self.display.clone(),
            })
        }

        pub(crate) fn try_clone_handle(&self) -> io::Result<File> {
            self.file.try_clone()
        }

        pub fn path(&self) -> &Path {
            &self.display
        }

        pub fn directory_fingerprint(&self) -> io::Result<ManagedDirFingerprint> {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(self.file.as_raw_fd(), stat.as_mut_ptr()) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let stat = unsafe { stat.assume_init() };
            let (modified_seconds, modified_nanos) = (stat.st_mtime, stat.st_mtime_nsec);
            Ok(ManagedDirFingerprint {
                device: stat.st_dev as u64,
                inode: stat.st_ino as u64,
                modified_seconds,
                modified_nanos,
            })
        }

        pub fn open_dir(&self, relative: impl AsRef<Path>) -> io::Result<Self> {
            let components = components(relative.as_ref())?;
            let mut current = self.file.try_clone()?;
            let mut display = self.display.clone();
            for component in components {
                let fd = unsafe {
                    libc::openat(
                        current.as_raw_fd(),
                        component.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                current = unsafe { File::from_raw_fd(fd) };
                display.push(
                    CStr::from_bytes_with_nul(component.as_bytes_with_nul())
                        .unwrap()
                        .to_string_lossy()
                        .as_ref(),
                );
            }
            Ok(Self {
                file: current,
                display,
            })
        }

        pub fn create_dir(&self, name: &str) -> io::Result<Self> {
            let name = leaf(name)?;
            let rc = unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) };
            if rc < 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                return Err(io::Error::last_os_error());
            }
            self.open_dir(Path::new(name.to_str().unwrap()))
        }

        pub fn open_file(&self, name: &str, write: bool) -> io::Result<File> {
            let name = leaf(name)?;
            let flags = if write { libc::O_RDWR } else { libc::O_RDONLY };
            self.open_file_flags(&name, flags)
        }

        pub fn open_new_file(&self, name: &str) -> io::Result<File> {
            let name = leaf(name)?;
            self.open_file_flags(&name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
        }

        pub fn open_or_create_file(&self, name: &str) -> io::Result<File> {
            let name = leaf(name)?;
            // Concurrent O_CREAT opens can return ENOENT on macOS when another
            // opener wins name creation. Retry by opening the existing leaf,
            // retaining no-follow and regular-file validation in both branches.
            match self.open_file_flags(&name, libc::O_RDWR | libc::O_CREAT) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.open_file_flags(&name, libc::O_RDWR)
                }
                result => result,
            }
        }

        fn open_file_flags(&self, name: &CString, flags: i32) -> io::Result<File> {
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                    0o600,
                )
            };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                let file = unsafe { File::from_raw_fd(fd) };
                let metadata = file.metadata()?;
                if !metadata.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "managed file is not regular",
                    ));
                }
                Ok(file)
            }
        }

        pub fn metadata(&self, name: &str) -> io::Result<Option<Entry>> {
            let name_c = leaf(name)?;
            self.metadata_c(&name_c, name)
        }

        fn metadata_c(&self, name: &CString, printable: &str) -> io::Result<Option<Entry>> {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            let rc = unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if rc < 0 {
                let error = io::Error::last_os_error();
                return if error.kind() == io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            let stat = unsafe { stat.assume_init() };
            let kind = stat.st_mode & libc::S_IFMT;
            Ok(Some(Entry {
                name: printable.into(),
                identity: Identity {
                    dev: stat.st_dev as u64,
                    ino: stat.st_ino as u64,
                },
                len: stat.st_size.max(0) as u64,
                allocated: (stat.st_blocks.max(0) as u64).saturating_mul(512),
                is_file: kind == libc::S_IFREG,
                is_dir: kind == libc::S_IFDIR,
            }))
        }

        pub fn rename_to(
            &self,
            name: &str,
            destination: &SecureDir,
            new_name: &str,
            expected: Identity,
        ) -> io::Result<()> {
            let source = self
                .metadata(name)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            if source.identity != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "managed entry identity changed before rename",
                ));
            }
            run_test_hook();
            let source = self
                .metadata(name)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            if source.identity != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "managed entry identity changed during rename",
                ));
            }
            let old = leaf(name)?;
            let new = leaf(new_name)?;
            if destination.metadata(new_name)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "managed rename destination exists",
                ));
            }
            let rc = unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    old.as_ptr(),
                    destination.file.as_raw_fd(),
                    new.as_ptr(),
                )
            };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            let moved = destination.metadata(new_name)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "renamed managed entry disappeared")
            })?;
            if moved.identity != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "managed entry identity changed during rename; reclamation refused",
                ));
            }
            self.sync()?;
            destination.sync()
        }

        pub fn replace_from(
            &self,
            name: &str,
            destination: &SecureDir,
            new_name: &str,
            expected_source: Identity,
            expected_destination: Option<Identity>,
        ) -> io::Result<()> {
            let source = self
                .metadata(name)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            if source.identity != expected_source
                || destination.metadata(new_name)?.map(|entry| entry.identity)
                    != expected_destination
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "managed replace identity changed",
                ));
            }
            let old = leaf(name)?;
            let new = leaf(new_name)?;
            let rc = unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    old.as_ptr(),
                    destination.file.as_raw_fd(),
                    new.as_ptr(),
                )
            };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            let moved = destination
                .metadata(new_name)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            if moved.identity != expected_source {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "managed source changed during replace",
                ));
            }
            self.sync()?;
            destination.sync()
        }

        pub fn remove_file(&self, name: &str, expected: Identity) -> io::Result<()> {
            self.unlink(name, expected, 0)
        }
        pub fn remove_dir(&self, name: &str, expected: Identity) -> io::Result<()> {
            self.unlink(name, expected, libc::AT_REMOVEDIR)
        }
        fn unlink(&self, name: &str, expected: Identity, flags: i32) -> io::Result<()> {
            let current = self
                .metadata(name)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            if current.identity != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "managed entry identity changed before deletion",
                ));
            }
            run_test_hook();
            let current = self
                .metadata(name)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            if current.identity != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "managed entry identity changed during deletion",
                ));
            }
            let name = leaf(name)?;
            let rc = unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), flags) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            self.sync()
        }

        pub fn read_dir_page(
            &self,
            cookie: Option<ManagedDirCursor>,
            limit: usize,
        ) -> io::Result<DirPage> {
            if limit == 0 {
                return Ok(DirPage {
                    entries: Vec::new(),
                    next_cookie: cookie,
                });
            }
            #[cfg(target_os = "macos")]
            return self.read_dir_page_macos(cookie, limit);
            #[cfg(not(target_os = "macos"))]
            {
                let cookie = match cookie {
                    Some(ManagedDirCursor::Unix(value)) => Some(value),
                    Some(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "directory cursor belongs to another platform",
                        ))
                    }
                    None => None,
                };
                #[cfg(not(target_os = "macos"))]
                let dot = c".";
                let fd = unsafe {
                    libc::openat(
                        self.file.as_raw_fd(),
                        dot.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                let stream = unsafe { libc::fdopendir(fd) };
                if stream.is_null() {
                    unsafe {
                        libc::close(fd);
                    }
                    return Err(io::Error::last_os_error());
                }
                if let Some(cookie) = cookie {
                    unsafe {
                        libc::seekdir(stream, cookie as libc::c_long);
                    }
                }
                let mut entries = Vec::new();
                let mut last_cookie = cookie.unwrap_or(0);
                while entries.len() < limit {
                    let ent = unsafe { libc::readdir(stream) };
                    if ent.is_null() {
                        break;
                    }
                    // Capture the resume cookie immediately after readdir. Some
                    // platforms report the start position again after unrelated
                    // descriptor-relative metadata calls.
                    #[cfg(target_os = "macos")]
                    {
                        last_cookie = unsafe { (*ent).d_seekoff as i64 };
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        last_cookie = unsafe { libc::telldir(stream) } as i64;
                    }
                    let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }
                        .to_string_lossy()
                        .into_owned();
                    if name != "." && name != ".." {
                        run_directory_metadata_test_hook();
                        let entry = self.metadata(&name)?.ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                "managed directory entry changed during enumeration",
                            )
                        })?;
                        entries.push(entry);
                    }
                }
                let next_cookie = if entries.len() == limit {
                    Some(ManagedDirCursor::Unix(last_cookie))
                } else {
                    None
                };
                unsafe {
                    libc::closedir(stream);
                }
                Ok(DirPage {
                    entries,
                    next_cookie,
                })
            }
        }

        #[cfg(target_os = "macos")]
        fn read_dir_page_macos(
            &self,
            cookie: Option<ManagedDirCursor>,
            limit: usize,
        ) -> io::Result<DirPage> {
            const BUFFER_BYTES: usize = 64 * 1024;
            unsafe extern "C" {
                #[link_name = "__getdirentries64"]
                fn getdirentries64(
                    fd: libc::c_int,
                    buf: *mut libc::c_char,
                    size: libc::size_t,
                    base: *mut libc::off_t,
                ) -> libc::ssize_t;
            }
            let (block_start, mut consumed) = match cookie {
                Some(ManagedDirCursor::MacOs {
                    kernel_block_start,
                    consumed_in_block,
                }) => (kernel_block_start, consumed_in_block as usize),
                Some(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "directory cursor belongs to another platform",
                    ))
                }
                None => (0, 0),
            };
            let dot = c".";
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            if unsafe { libc::lseek(fd, block_start, libc::SEEK_SET) } < 0 {
                unsafe {
                    libc::close(fd);
                }
                return Err(io::Error::last_os_error());
            }
            let mut entries = Vec::new();
            let mut buffer = vec![0u8; BUFFER_BYTES];
            let mut current_block = block_start;
            loop {
                let mut base: i64 = 0;
                let bytes = unsafe {
                    getdirentries64(fd, buffer.as_mut_ptr().cast(), buffer.len(), &mut base)
                };
                if bytes < 0 {
                    unsafe {
                        libc::close(fd);
                    }
                    return Err(io::Error::last_os_error());
                }
                if bytes == 0 {
                    unsafe {
                        libc::close(fd);
                    }
                    return Ok(DirPage {
                        entries,
                        next_cookie: None,
                    });
                }
                let next_block = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) };
                if next_block < 0 {
                    unsafe {
                        libc::close(fd);
                    }
                    return Err(io::Error::last_os_error());
                }
                let mut offset = 0usize;
                let mut record = 0usize;
                while offset < bytes as usize {
                    if offset + 21 > bytes as usize {
                        unsafe {
                            libc::close(fd);
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "truncated Darwin directory record",
                        ));
                    }
                    let reclen =
                        u16::from_ne_bytes([buffer[offset + 16], buffer[offset + 17]]) as usize;
                    let namlen =
                        u16::from_ne_bytes([buffer[offset + 18], buffer[offset + 19]]) as usize;
                    if reclen < 21 || offset + reclen > bytes as usize || namlen > reclen - 21 {
                        unsafe {
                            libc::close(fd);
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid Darwin directory record",
                        ));
                    }
                    record += 1;
                    if record > consumed {
                        let name = std::str::from_utf8(&buffer[offset + 21..offset + 21 + namlen])
                            .map_err(|_| {
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "non-UTF-8 managed directory entry",
                                )
                            })?
                            .to_owned();
                        if name != "." && name != ".." {
                            run_directory_metadata_test_hook();
                            let entry = self.metadata(&name)?.ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::NotFound,
                                    "managed directory entry changed during enumeration",
                                )
                            })?;
                            entries.push(entry);
                            if entries.len() == limit {
                                unsafe {
                                    libc::close(fd);
                                }
                                return Ok(DirPage {
                                    entries,
                                    next_cookie: Some(ManagedDirCursor::MacOs {
                                        kernel_block_start: current_block,
                                        consumed_in_block: record as u32,
                                    }),
                                });
                            }
                        }
                    }
                    offset += reclen;
                }
                current_block = next_block;
                consumed = 0;
            }
        }

        pub fn sync(&self) -> io::Result<()> {
            self.file.sync_all()
        }
        pub fn identity(&self) -> io::Result<Identity> {
            Self::file_identity(&self.file)
        }
        pub fn file_identity(file: &File) -> io::Result<Identity> {
            let m = file.metadata()?;
            use std::os::unix::fs::MetadataExt;
            Ok(Identity {
                dev: m.dev(),
                ino: m.ino(),
            })
        }
    }

    fn components(path: &Path) -> io::Result<Vec<CString>> {
        path.components()
            .map(|c| match c {
                Component::Normal(v) => cstring(v.as_bytes()),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "managed path must be relative and contain only normal components",
                )),
            })
            .collect()
    }
    fn leaf(name: &str) -> io::Result<CString> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed entry name is invalid",
            ));
        }
        cstring(name.as_bytes())
    }
    fn cstring(bytes: &[u8]) -> io::Result<CString> {
        CString::new(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "managed path contains NUL"))
    }
    #[cfg(test)]
    pub(crate) fn set_before_mutation_hook(hook: impl FnOnce() + 'static) {
        BEFORE_MUTATION.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }
    #[cfg(test)]
    fn run_test_hook() {
        if let Some(h) = BEFORE_MUTATION.with(|slot| slot.borrow_mut().take()) {
            h();
        }
    }
    #[cfg(not(test))]
    fn run_test_hook() {}

    #[cfg(test)]
    pub(crate) fn set_before_directory_metadata_hook(hook: impl FnOnce() + 'static) {
        BEFORE_DIRECTORY_METADATA.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }
    #[cfg(test)]
    fn run_directory_metadata_test_hook() {
        BEFORE_DIRECTORY_METADATA.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
    }
    #[cfg(not(test))]
    fn run_directory_metadata_test_hook() {}
}

#[cfg(windows)]
mod platform {
    use super::{ManagedDirCursor, ManagedDirFingerprint};
    use std::{
        ffi::c_void,
        fs::File,
        io,
        mem::{size_of, zeroed},
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        path::{Component, Path, PathBuf},
        ptr,
        sync::Mutex,
    };
    use windows_sys::Win32::{
        Foundation::{BOOLEAN, HANDLE},
        Storage::FileSystem::{
            FileDispositionInfo, FileRenameInfo, FlushFileBuffers, GetFileInformationByHandle,
            SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_DISPOSITION_INFO,
            FILE_RENAME_INFO,
        },
    };
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
    #[repr(C)]
    struct DirInfo {
        next: u32,
        index: u32,
        creation: i64,
        access: i64,
        write: i64,
        change: i64,
        end: i64,
        allocation: i64,
        attributes: u32,
        name_len: u32,
        ea: u32,
        short_len: u8,
        _pad: u8,
        short: [u16; 12],
        file_id: i64,
        name: [u16; 1],
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
        fn NtQueryDirectoryFile(
            handle: HANDLE,
            event: HANDLE,
            apc: *mut c_void,
            context: *mut c_void,
            status: *mut IoStatus,
            info: *mut c_void,
            length: u32,
            class: i32,
            single: BOOLEAN,
            name: *mut UnicodeString,
            restart: BOOLEAN,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }
    const SYNC: u32 = 0x100000;
    const DELETE: u32 = 0x10000;
    const READ_ATTR: u32 = 0x80;
    const WRITE_ATTR: u32 = 0x100;
    const READ: u32 = 1;
    const WRITE: u32 = 2;
    const OPEN: u32 = 1;
    const CREATE: u32 = 2;
    const OPEN_IF: u32 = 3;
    const DIR: u32 = 1;
    const NONDIR: u32 = 0x40;
    const SYNC_IO: u32 = 0x20;
    const REPARSE: u32 = 0x200000;
    const ATTR_DIR: u32 = 0x10;
    const ATTR_REPARSE: u32 = 0x400;
    const STATUS_NO_MORE_FILES: i32 = 0x80000006u32 as i32;
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Identity {
        pub dev: u64,
        pub ino: u64,
    }
    #[derive(Debug, Clone)]
    pub struct Entry {
        pub name: String,
        pub identity: Identity,
        pub len: u64,
        pub allocated: u64,
        pub is_file: bool,
        pub is_dir: bool,
    }
    #[derive(Debug)]
    pub struct DirPage {
        pub entries: Vec<Entry>,
        pub next_cookie: Option<ManagedDirCursor>,
    }
    #[derive(Debug)]
    pub struct SecureDir {
        file: File,
        display: PathBuf,
        cursor: std::sync::Arc<Mutex<i64>>,
    }
    impl SecureDir {
        pub fn open(path: &Path) -> io::Result<Self> {
            use std::os::windows::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(0x02000000 | REPARSE)
                .open(path)?;
            validate_kind(&file, true)?;
            Ok(Self {
                file,
                display: path.canonicalize()?,
                cursor: std::sync::Arc::new(Mutex::new(0)),
            })
        }
        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                file: self.file.try_clone()?,
                display: self.display.clone(),
                cursor: self.cursor.clone(),
            })
        }

        pub(crate) fn try_clone_handle(&self) -> io::Result<File> {
            self.file.try_clone()
        }

        pub fn path(&self) -> &Path {
            &self.display
        }
        pub fn directory_fingerprint(&self) -> io::Result<ManagedDirFingerprint> {
            let info = handle_info(&self.file)?;
            let ticks = ((info.ftLastWriteTime.dwHighDateTime as u64) << 32)
                | info.ftLastWriteTime.dwLowDateTime as u64;
            Ok(ManagedDirFingerprint {
                device: info.dwVolumeSerialNumber as u64,
                inode: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
                modified_seconds: (ticks / 10_000_000) as i64,
                modified_nanos: ((ticks % 10_000_000) * 100) as i64,
            })
        }
        pub fn open_dir(&self, path: impl AsRef<Path>) -> io::Result<Self> {
            let mut file = self.file.try_clone()?;
            let mut display = self.display.clone();
            for name in components(path.as_ref())? {
                file = open_relative(&file, &name, READ | WRITE | READ_ATTR, OPEN, DIR)?;
                display.push(String::from_utf16_lossy(&name));
            }
            Ok(Self {
                file,
                display,
                cursor: std::sync::Arc::new(Mutex::new(0)),
            })
        }
        pub fn create_dir(&self, name: &str) -> io::Result<Self> {
            let wide = leaf(name)?;
            let file = match open_relative(&self.file, &wide, READ | WRITE | READ_ATTR, CREATE, DIR)
            {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    open_relative(&self.file, &wide, READ | WRITE | READ_ATTR, OPEN, DIR)?
                }
                Err(e) => return Err(e),
            };
            let mut display = self.display.clone();
            display.push(name);
            Ok(Self {
                file,
                display,
                cursor: std::sync::Arc::new(Mutex::new(0)),
            })
        }
        pub fn open_file(&self, name: &str, write: bool) -> io::Result<File> {
            open_relative(
                &self.file,
                &leaf(name)?,
                READ | READ_ATTR | if write { WRITE | WRITE_ATTR } else { 0 },
                OPEN,
                NONDIR,
            )
        }
        pub fn open_new_file(&self, name: &str) -> io::Result<File> {
            open_relative(
                &self.file,
                &leaf(name)?,
                WRITE | READ_ATTR | WRITE_ATTR | DELETE,
                CREATE,
                NONDIR,
            )
        }
        pub fn open_or_create_file(&self, name: &str) -> io::Result<File> {
            open_relative(
                &self.file,
                &leaf(name)?,
                READ | WRITE | READ_ATTR | WRITE_ATTR | DELETE,
                OPEN_IF,
                NONDIR,
            )
        }
        pub fn metadata(&self, name: &str) -> io::Result<Option<Entry>> {
            match open_relative(&self.file, &leaf(name)?, READ_ATTR, OPEN, 0) {
                Ok(f) => Ok(Some(entry(&f, name)?)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }
        pub fn rename_to(
            &self,
            name: &str,
            dest: &Self,
            new: &str,
            expected: Identity,
        ) -> io::Result<()> {
            if dest.metadata(new)?.is_some() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            self.rename(name, dest, new, expected, false)
        }
        pub fn replace_from(
            &self,
            name: &str,
            dest: &Self,
            new: &str,
            expected: Identity,
            old: Option<Identity>,
        ) -> io::Result<()> {
            if dest.metadata(new)?.map(|e| e.identity) != old {
                return Err(changed());
            }
            self.rename(name, dest, new, expected, true)
        }
        fn rename(
            &self,
            name: &str,
            dest: &Self,
            new: &str,
            expected: Identity,
            replace: bool,
        ) -> io::Result<()> {
            let source = open_relative(&self.file, &leaf(name)?, READ_ATTR | DELETE, OPEN, 0)?;
            if identity(&source)? != expected {
                return Err(changed());
            }
            let wide = leaf(new)?;
            let head = size_of::<FILE_RENAME_INFO>() - 2;
            let size = head + wide.len() * 2;
            let mut storage = vec![0u64; (size + 7) / 8];
            let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            unsafe {
                (*info).Anonymous.ReplaceIfExists = replace as u8;
                (*info).RootDirectory = dest.file.as_raw_handle() as HANDLE;
                (*info).FileNameLength = (wide.len() * 2) as u32;
                ptr::copy_nonoverlapping(wide.as_ptr(), (*info).FileName.as_mut_ptr(), wide.len());
                if SetFileInformationByHandle(
                    source.as_raw_handle() as HANDLE,
                    FileRenameInfo,
                    info.cast(),
                    size as u32,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            if dest.metadata(new)?.map(|e| e.identity) != Some(expected) {
                return Err(changed());
            }
            self.sync()?;
            dest.sync()
        }
        pub fn remove_file(&self, name: &str, id: Identity) -> io::Result<()> {
            self.remove(name, id, false)
        }
        pub fn remove_dir(&self, name: &str, id: Identity) -> io::Result<()> {
            self.remove(name, id, true)
        }
        fn remove(&self, name: &str, id: Identity, directory: bool) -> io::Result<()> {
            let file = open_relative(
                &self.file,
                &leaf(name)?,
                READ_ATTR | DELETE,
                OPEN,
                if directory { DIR } else { NONDIR },
            )?;
            if identity(&file)? != id {
                return Err(changed());
            }
            let info = FILE_DISPOSITION_INFO { DeleteFile: 1 };
            unsafe {
                if SetFileInformationByHandle(
                    file.as_raw_handle() as HANDLE,
                    FileDispositionInfo,
                    (&info as *const FILE_DISPOSITION_INFO).cast(),
                    size_of::<FILE_DISPOSITION_INFO>() as u32,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            drop(file);
            self.sync()
        }
        pub fn read_dir_page(
            &self,
            cookie: Option<ManagedDirCursor>,
            limit: usize,
        ) -> io::Result<DirPage> {
            let mut cursor = self.cursor.lock().unwrap();
            let requested = match cookie {
                Some(ManagedDirCursor::Windows(value)) => value as i64,
                Some(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "directory cursor belongs to another platform",
                    ))
                }
                None => 0,
            };
            if requested != *cursor {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stale managed directory cursor",
                ));
            }
            let mut entries = Vec::new();
            let mut restart = (requested == 0) as u8;
            let mut ended = false;
            while entries.len() < limit {
                let mut buf = [0u64; 80];
                let mut ios: IoStatus = unsafe { zeroed() };
                let status = unsafe {
                    NtQueryDirectoryFile(
                        self.file.as_raw_handle() as HANDLE,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &mut ios,
                        buf.as_mut_ptr().cast(),
                        (buf.len() * 8) as u32,
                        37,
                        1,
                        ptr::null_mut(),
                        restart,
                    )
                };
                restart = 0;
                if status == STATUS_NO_MORE_FILES {
                    ended = true;
                    break;
                }
                if status < 0 {
                    return Err(nt_error(status));
                }
                let info = unsafe { &*buf.as_ptr().cast::<DirInfo>() };
                let name = String::from_utf16_lossy(unsafe {
                    std::slice::from_raw_parts(info.name.as_ptr(), info.name_len as usize / 2)
                });
                if name != "." && name != ".." {
                    entries.push(
                        self.metadata(&name)?
                            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?,
                    );
                }
            }
            *cursor = if ended { 0 } else { *cursor + 1 };
            Ok(DirPage {
                next_cookie: if !ended && entries.len() == limit {
                    Some(ManagedDirCursor::Windows(*cursor as u64))
                } else {
                    None
                },
                entries,
            })
        }

        pub fn sync(&self) -> io::Result<()> {
            unsafe {
                if FlushFileBuffers(self.file.as_raw_handle() as HANDLE) == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        }
        pub fn identity(&self) -> io::Result<Identity> {
            identity(&self.file)
        }
        pub fn file_identity(file: &File) -> io::Result<Identity> {
            identity(file)
        }
    }
    fn open_relative(
        root: &File,
        name: &[u16],
        access: u32,
        disposition: u32,
        kind: u32,
    ) -> io::Result<File> {
        let mut owned = name.to_vec();
        let mut unicode = UnicodeString {
            length: (owned.len() * 2) as u16,
            maximum_length: (owned.len() * 2) as u16,
            buffer: owned.as_mut_ptr(),
        };
        let mut attrs = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root: root.as_raw_handle() as HANDLE,
            name: &mut unicode,
            attributes: 0x40,
            security: ptr::null_mut(),
            qos: ptr::null_mut(),
        };
        let mut ios: IoStatus = unsafe { zeroed() };
        let mut handle: HANDLE = ptr::null_mut();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access | SYNC,
                &mut attrs,
                &mut ios,
                ptr::null_mut(),
                0x80,
                7,
                disposition,
                kind | SYNC_IO | REPARSE,
                ptr::null_mut(),
                0,
            )
        };
        if status < 0 {
            return Err(nt_error(status));
        }
        let file = unsafe { File::from_raw_handle(handle) };
        if handle_info(&file)?.dwFileAttributes & ATTR_REPARSE != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed reparse point refused",
            ));
        }
        Ok(file)
    }
    fn handle_info(f: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
        let mut i = unsafe { zeroed() };
        unsafe {
            if GetFileInformationByHandle(f.as_raw_handle() as HANDLE, &mut i) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(i)
    }
    fn identity(f: &File) -> io::Result<Identity> {
        let i = handle_info(f)?;
        Ok(Identity {
            dev: i.dwVolumeSerialNumber as u64,
            ino: ((i.nFileIndexHigh as u64) << 32) | i.nFileIndexLow as u64,
        })
    }
    fn validate_kind(f: &File, dir: bool) -> io::Result<()> {
        let a = handle_info(f)?.dwFileAttributes;
        if a & ATTR_REPARSE != 0 || (a & ATTR_DIR != 0) != dir {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed entry has invalid type",
            ));
        }
        Ok(())
    }
    fn entry(f: &File, name: &str) -> io::Result<Entry> {
        let i = handle_info(f)?;
        let a = i.dwFileAttributes;
        let len = ((i.nFileSizeHigh as u64) << 32) | i.nFileSizeLow as u64;
        Ok(Entry {
            name: name.into(),
            identity: identity(f)?,
            len,
            allocated: len,
            is_file: a & (ATTR_DIR | ATTR_REPARSE) == 0,
            is_dir: a & ATTR_DIR != 0 && a & ATTR_REPARSE == 0,
        })
    }
    fn components(p: &Path) -> io::Result<Vec<Vec<u16>>> {
        p.components()
            .map(|c| match c {
                Component::Normal(v) => Ok(v.encode_wide().collect()),
                _ => Err(invalid()),
            })
            .collect()
    }
    fn leaf(n: &str) -> io::Result<Vec<u16>> {
        if n.is_empty() || n == "." || n == ".." || n.contains('/') || n.contains('\\') {
            return Err(invalid());
        }
        let v: Vec<_> = std::ffi::OsStr::new(n).encode_wide().collect();
        if v.contains(&0) {
            Err(invalid())
        } else {
            Ok(v)
        }
    }
    fn invalid() -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, "managed entry name is invalid")
    }
    fn changed() -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, "managed entry identity changed")
    }
    fn nt_error(s: i32) -> io::Error {
        io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(s) } as i32)
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("managed filesystem requires Unix or Windows descriptor-relative APIs");

#[cfg(all(test, unix))]
pub(crate) use platform::set_before_directory_metadata_hook;
#[cfg(all(test, unix))]
pub(crate) use platform::set_before_mutation_hook;
pub use platform::{
    DirPage as ManagedDirPage, Entry as ManagedEntry, Identity as ManagedIdentity, SecureDir,
};

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[cfg(target_os = "macos")]
    #[test]
    fn native_cursor_reopens_and_progresses_beyond_sixty_five_thousand_entries() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..65_600u32 {
            fs::File::create(root.path().join(format!("entry-{index:05}"))).unwrap();
        }
        let long_name = format!("long-{}", "x".repeat(230));
        fs::File::create(root.path().join(&long_name)).unwrap();
        let mut cursor = None;
        let mut names = std::collections::BTreeSet::new();
        loop {
            // Persisted maintenance reopens the pinned directory between runs.
            let directory = SecureDir::open(root.path()).unwrap();
            let page = directory.read_dir_page(cursor.clone(), 256).unwrap();
            assert!(page.entries.len() <= 256);
            names.extend(page.entries.into_iter().map(|entry| entry.name));
            cursor = page.next_cookie;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(names.len(), 65_601);
        assert!(names.contains(&long_name));
    }
    #[test]
    fn concurrent_create_or_open_uses_the_same_regular_file() {
        let root = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let directory = SecureDir::open(root.path()).unwrap();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    for index in 0..32 {
                        barrier.wait();
                        let file = directory
                            .open_or_create_file(&format!("lock-{index}"))
                            .unwrap();
                        assert!(file.metadata().unwrap().is_file());
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 32);
    }

    #[test]
    fn swap_between_identity_check_and_delete_cannot_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("protected"), b"keep").unwrap();
        fs::write(root.path().join("victim"), b"managed").unwrap();
        let dir = SecureDir::open(root.path()).unwrap();
        let identity = dir.metadata("victim").unwrap().unwrap().identity;
        let victim = root.path().join("victim");
        let parked = root.path().join("parked");
        let external = outside.path().join("protected");
        set_before_mutation_hook(move || {
            fs::rename(&victim, &parked).unwrap();
            symlink(&external, &victim).unwrap();
        });
        assert!(dir.remove_file("victim", identity).is_err());
        assert_eq!(fs::read(outside.path().join("protected")).unwrap(), b"keep");
        assert_eq!(fs::read(root.path().join("parked")).unwrap(), b"managed");
    }

    #[test]
    fn mutation_test_hooks_are_isolated_per_thread() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        fs::write(first.path().join("one"), b"one").unwrap();
        fs::write(second.path().join("two"), b"two").unwrap();
        let first_dir = SecureDir::open(first.path()).unwrap();
        let first_id = first_dir.metadata("one").unwrap().unwrap().identity;
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = calls.clone();
        set_before_mutation_hook(move || {
            hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let second_path = second.path().to_path_buf();
        std::thread::spawn(move || {
            let dir = SecureDir::open(&second_path).unwrap();
            let id = dir.metadata("two").unwrap().unwrap().identity;
            dir.remove_file("two", id).unwrap();
        })
        .join()
        .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        first_dir.remove_file("one", first_id).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
