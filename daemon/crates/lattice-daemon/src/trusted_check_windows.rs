//! Windows process containment for explicit trusted checks.

use super::{DeclaredCheck, Execution, TrustedCheckError, MAX_OUTPUT_BYTES};
use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io::Read;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_SHARE_READ, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE, operation: &str) -> std::io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(std::io::Error::new(
                std::io::Error::last_os_error().kind(),
                format!("{operation}: {}", std::io::Error::last_os_error()),
            ))
        } else {
            Ok(Self(handle))
        }
    }

    fn take(&mut self) -> HANDLE {
        std::mem::replace(&mut self.0, std::ptr::null_mut())
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

unsafe impl Send for OwnedHandle {}

struct AttributeList {
    _bytes: Vec<u8>,
    pointer: *mut c_void,
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            unsafe { DeleteProcThreadAttributeList(self.pointer) };
        }
    }
}

pub(super) fn execute(
    check: &DeclaredCheck,
    root: &Path,
    timeout: Duration,
) -> Result<Execution, TrustedCheckError> {
    execute_inner(check, root, timeout).map_err(|source| {
        if source.kind() == std::io::ErrorKind::TimedOut {
            TrustedCheckError::Timeout {
                check_id: check.id.clone(),
                timeout_ms: timeout.as_millis().try_into().unwrap_or(u64::MAX),
            }
        } else {
            TrustedCheckError::Execution {
                check_id: check.id.clone(),
                source,
            }
        }
    })
}

fn execute_inner(
    check: &DeclaredCheck,
    root: &Path,
    timeout: Duration,
) -> std::io::Result<Execution> {
    let executable = if check.argv[0].starts_with("./") {
        root.join(&check.argv[0][2..])
    } else {
        PathBuf::from(&check.argv[0])
    };
    let application = wide_nul(executable.as_os_str())?;
    let current_directory = wide_nul(root.as_os_str())?;
    let mut command_line = command_line(&check.argv)?;
    let environment = environment_block(&check.env)?;

    let mut security: SECURITY_ATTRIBUTES = unsafe { zeroed() };
    security.nLength = size_of::<SECURITY_ATTRIBUTES>() as u32;
    security.bInheritHandle = 1;
    let (mut stdout_read, stdout_write) = pipe(&security)?;
    let (mut stderr_read, stderr_write) = pipe(&security)?;
    let stdin = open_null(&security)?;
    for handle in [stdout_read.0, stderr_read.0] {
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(last("make pipe reader private"));
        }
    }

    let mut inherited = [stdin.0, stdout_write.0, stderr_write.0];
    let attributes = attribute_list(&mut inherited)?;
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin.0;
    startup.StartupInfo.hStdOutput = stdout_write.0;
    startup.StartupInfo.hStdError = stderr_write.0;
    startup.lpAttributeList = attributes.pointer;

    let job = OwnedHandle::new(
        unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) },
        "create Job Object",
    )?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(last("configure Job Object"));
    }

    let mut process: PROCESS_INFORMATION = unsafe { zeroed() };
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr() as *const c_void,
            current_directory.as_ptr(),
            &startup.StartupInfo,
            &mut process,
        )
    } == 0
    {
        return Err(last("create suspended trusted check"));
    }
    let process_handle = OwnedHandle::new(process.hProcess, "capture process handle")?;
    let thread_handle = OwnedHandle::new(process.hThread, "capture thread handle")?;

    // No repository code has run yet. Every failure below terminates and reaps
    // the suspended process before returning.
    if unsafe { AssignProcessToJobObject(job.0, process_handle.0) } == 0 {
        let error = last("assign suspended trusted check to Job Object");
        unsafe { TerminateProcess(process_handle.0, 126) };
        unsafe { WaitForSingleObject(process_handle.0, 30_000) };
        return Err(error);
    }
    if unsafe { ResumeThread(thread_handle.0) } == u32::MAX {
        let error = last("resume contained trusted check");
        unsafe { TerminateJobObject(job.0, 126) };
        unsafe { WaitForSingleObject(process_handle.0, 30_000) };
        return Err(error);
    }
    drop(thread_handle);
    drop(attributes);
    drop(stdin);
    drop(stdout_write);
    drop(stderr_write);

    let overflow = Arc::new(AtomicBool::new(false));
    let stdout = reader(stdout_read.take(), overflow.clone());
    let stderr = reader(stderr_read.take(), overflow.clone());
    let deadline = Instant::now() + timeout;
    let mut completion_error = None;
    loop {
        let wait = unsafe { WaitForSingleObject(process_handle.0, 10) };
        if wait == WAIT_OBJECT_0 {
            break;
        }
        if wait != WAIT_TIMEOUT {
            let error = last("wait for trusted check");
            unsafe { TerminateJobObject(job.0, 126) };
            unsafe { WaitForSingleObject(process_handle.0, 30_000) };
            completion_error = Some(error);
            break;
        }
        if overflow.load(Ordering::Acquire) {
            unsafe { TerminateJobObject(job.0, 126) };
            unsafe { WaitForSingleObject(process_handle.0, 30_000) };
            completion_error = Some(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("check output exceeded {MAX_OUTPUT_BYTES} bytes per stream"),
            ));
            break;
        }
        if Instant::now() >= deadline {
            unsafe { TerminateJobObject(job.0, 124) };
            unsafe { WaitForSingleObject(process_handle.0, 30_000) };
            completion_error = Some(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "trusted check timed out after Job Object termination",
            ));
            break;
        }
    }
    // Closing the kill-on-close job after the primary exits also removes any
    // descendants that outlived it. Keep the process handle until exit code is read.
    drop(job);
    let mut exit_code = 0u32;
    let exit_code_error = (unsafe { GetExitCodeProcess(process_handle.0, &mut exit_code) } == 0)
        .then(|| last("read trusted check exit status"));
    drop(process_handle);
    let stdout = stdout
        .join()
        .map_err(|_| std::io::Error::other("trusted check stdout reader panicked"))??;
    let stderr = stderr
        .join()
        .map_err(|_| std::io::Error::other("trusted check stderr reader panicked"))??;
    if let Some(error) = completion_error {
        return Err(error);
    }
    if let Some(error) = exit_code_error {
        return Err(error);
    }
    if stdout.len() > MAX_OUTPUT_BYTES || stderr.len() > MAX_OUTPUT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("check output exceeded {MAX_OUTPUT_BYTES} bytes per stream"),
        ));
    }
    use std::os::windows::process::ExitStatusExt;
    Ok(Execution {
        status: ExitStatus::from_raw(exit_code),
        stdout,
        stderr,
    })
}

fn pipe(security: &SECURITY_ATTRIBUTES) -> std::io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, security, 64 * 1024) } == 0 {
        return Err(last("create output pipe"));
    }
    Ok((
        OwnedHandle::new(read, "pipe reader")?,
        OwnedHandle::new(write, "pipe writer")?,
    ))
}

fn open_null(security: &SECURITY_ATTRIBUTES) -> std::io::Result<OwnedHandle> {
    let name = wide_nul(OsStr::new("NUL"))?;
    OwnedHandle::new(
        unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ,
                security,
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        },
        "open NUL stdin",
    )
}

fn attribute_list(handles: &mut [HANDLE]) -> std::io::Result<AttributeList> {
    let mut size = 0usize;
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
    if size == 0 {
        return Err(last("size process attribute list"));
    }
    let mut bytes = vec![0u8; size];
    let pointer = bytes.as_mut_ptr() as *mut c_void;
    if unsafe { InitializeProcThreadAttributeList(pointer, 1, 0, &mut size) } == 0 {
        return Err(last("initialize process attribute list"));
    }
    let list = AttributeList {
        _bytes: bytes,
        pointer,
    };
    if unsafe {
        UpdateProcThreadAttribute(
            list.pointer,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr() as *const c_void,
            std::mem::size_of_val(handles),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(last("restrict inherited handles"));
    }
    Ok(list)
}

fn reader(
    handle: HANDLE,
    overflow: Arc<AtomicBool>,
) -> thread::JoinHandle<std::io::Result<Vec<u8>>> {
    // Raw Windows handles are pointer aliases and therefore not `Send`. The
    // integer value is an owned kernel handle transferred to the reader.
    let handle = handle as usize;
    thread::spawn(move || {
        let mut file = unsafe { File::from_raw_handle(handle as *mut c_void) };
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_OUTPUT_BYTES {
            overflow.store(true, Ordering::Release);
        }
        Ok(bytes)
    })
}

fn command_line(argv: &[String]) -> std::io::Result<Vec<u16>> {
    let mut line = String::new();
    for (index, arg) in argv.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        quote_argument(arg, &mut line);
    }
    wide_nul(OsStr::new(&line))
}

fn quote_argument(arg: &str, output: &mut String) {
    output.push('"');
    let mut slashes = 0usize;
    for character in arg.chars() {
        match character {
            '\\' => slashes += 1,
            '"' => {
                output.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                output.push('"');
                slashes = 0;
            }
            _ => {
                output.extend(std::iter::repeat_n('\\', slashes));
                slashes = 0;
                output.push(character);
            }
        }
    }
    output.extend(std::iter::repeat_n('\\', slashes * 2));
    output.push('"');
}

fn environment_block(
    env: &std::collections::BTreeMap<String, String>,
) -> std::io::Result<Vec<u16>> {
    let mut block = Vec::new();
    for (key, value) in env {
        block.extend(wide(OsStr::new(&format!("{key}={value}")))?);
        block.push(0);
    }
    block.push(0);
    if env.is_empty() {
        block.push(0);
    }
    Ok(block)
}

fn wide(value: &OsStr) -> std::io::Result<Vec<u16>> {
    let encoded: Vec<u16> = value.encode_wide().collect();
    if encoded.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Windows process value contains NUL",
        ));
    }
    Ok(encoded)
}

fn wide_nul(value: &OsStr) -> std::io::Result<Vec<u16>> {
    let mut encoded = wide(value)?;
    encoded.push(0);
    Ok(encoded)
}

fn last(operation: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::Error::last_os_error().kind(),
        format!("{operation}: {}", std::io::Error::last_os_error()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_windows_arguments_without_shell_parsing() {
        let mut quoted = String::new();
        quote_argument(r#"a b\"c\\"#, &mut quoted);
        assert_eq!(quoted, r#""a b\"c\\""#);
    }
}
