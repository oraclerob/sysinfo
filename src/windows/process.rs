// Take a look at the license at the top of the repository in the LICENSE file.

use crate::{DiskUsage, Pid, ProcessExt, ProcessStatus, Signal};

use std::ffi::OsString;
use std::fmt;
use std::mem::{size_of, zeroed, MaybeUninit};
use std::ops::Deref;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process;
use std::ptr::null_mut;
use std::str;

use libc::{c_void, memcpy};

use ntapi::ntpebteb::PEB;
use ntapi::ntwow64::{PEB32, PRTL_USER_PROCESS_PARAMETERS32, RTL_USER_PROCESS_PARAMETERS32};
use once_cell::sync::Lazy;

use ntapi::ntpsapi::{
    NtQueryInformationProcess, ProcessBasicInformation, ProcessCommandLineInformation,
    ProcessWow64Information, PROCESSINFOCLASS, PROCESS_BASIC_INFORMATION,
};
use ntapi::ntrtl::{RtlGetVersion, PRTL_USER_PROCESS_PARAMETERS, RTL_USER_PROCESS_PARAMETERS};
use winapi::shared::basetsd::SIZE_T;
use winapi::shared::minwindef::{DWORD, FALSE, FILETIME, LPVOID, MAX_PATH, TRUE, ULONG};
use winapi::shared::ntdef::{NT_SUCCESS, UNICODE_STRING};
use winapi::shared::ntstatus::{
    STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL, STATUS_INFO_LENGTH_MISMATCH,
};
use winapi::um::handleapi::CloseHandle;
use winapi::um::memoryapi::{ReadProcessMemory, VirtualQueryEx};
use winapi::um::processthreadsapi::{GetProcessTimes, GetSystemTimes, OpenProcess};
use winapi::um::psapi::{
    EnumProcessModulesEx, GetModuleBaseNameW, GetModuleFileNameExW, GetProcessMemoryInfo,
    LIST_MODULES_ALL, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use winapi::um::winbase::{GetProcessIoCounters, CREATE_NO_WINDOW};
use winapi::um::winnt::{
    HANDLE, IO_COUNTERS, MEMORY_BASIC_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    RTL_OSVERSIONINFOEXW, ULARGE_INTEGER,
};

impl fmt::Display for ProcessStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match *self {
            ProcessStatus::Run => "Runnable",
            _ => "Unknown",
        })
    }
}

fn get_process_handler(pid: Pid) -> Option<HANDLE> {
    if pid.0 == 0 {
        return None;
    }
    let options = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ;

    unsafe {
        let process_handler = OpenProcess(options, FALSE, pid.0 as DWORD);
        if process_handler.is_null() {
            None
        } else {
            Some(process_handler)
        }
    }
}

#[derive(Clone)]
struct PtrWrapper<T: Clone>(T);

impl<T: Clone> Deref for PtrWrapper<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T: Clone> Send for PtrWrapper<T> {}
unsafe impl<T: Clone> Sync for PtrWrapper<T> {}

#[doc = include_str!("../../md_doc/process.md")]
pub struct Process {
    name: String,
    cmd: Vec<String>,
    exe: PathBuf,
    pid: Pid,
    environ: Vec<String>,
    cwd: PathBuf,
    root: PathBuf,
    pub(crate) memory: u64,
    pub(crate) virtual_memory: u64,
    parent: Option<Pid>,
    status: ProcessStatus,
    handle: PtrWrapper<HANDLE>,
    cpu_calc_values: CPUsageCalculationValues,
    start_time: u64,
    pub(crate) run_time: u64,
    cpu_usage: f32,
    pub(crate) updated: bool,
    old_read_bytes: u64,
    old_written_bytes: u64,
    read_bytes: u64,
    written_bytes: u64,
}

struct CPUsageCalculationValues {
    old_process_sys_cpu: u64,
    old_process_user_cpu: u64,
    old_system_sys_cpu: u64,
    old_system_user_cpu: u64,
}

impl CPUsageCalculationValues {
    fn new() -> Self {
        CPUsageCalculationValues {
            old_process_sys_cpu: 0,
            old_process_user_cpu: 0,
            old_system_sys_cpu: 0,
            old_system_user_cpu: 0,
        }
    }
}
static WINDOWS_8_1_OR_NEWER: Lazy<bool> = Lazy::new(|| unsafe {
    let mut version_info: RTL_OSVERSIONINFOEXW = MaybeUninit::zeroed().assume_init();

    version_info.dwOSVersionInfoSize = std::mem::size_of::<RTL_OSVERSIONINFOEXW>() as u32;
    if !NT_SUCCESS(RtlGetVersion(
        &mut version_info as *mut RTL_OSVERSIONINFOEXW as *mut _,
    )) {
        return true;
    }

    // Windows 8.1 is 6.3
    version_info.dwMajorVersion > 6
        || version_info.dwMajorVersion == 6 && version_info.dwMinorVersion >= 3
});

unsafe fn get_process_name(process_handler: HANDLE, h_mod: *mut c_void) -> String {
    let mut process_name = [0u16; MAX_PATH + 1];

    GetModuleBaseNameW(
        process_handler,
        h_mod as _,
        process_name.as_mut_ptr(),
        MAX_PATH as DWORD + 1,
    );
    null_terminated_wchar_to_string(&process_name)
}

unsafe fn get_h_mod(process_handler: HANDLE, h_mod: &mut *mut c_void) -> bool {
    let mut cb_needed = 0;
    EnumProcessModulesEx(
        process_handler,
        h_mod as *mut *mut c_void as _,
        size_of::<DWORD>() as DWORD,
        &mut cb_needed,
        LIST_MODULES_ALL,
    ) != 0
}

unsafe fn get_exe(process_handler: HANDLE, h_mod: *mut c_void) -> PathBuf {
    let mut exe_buf = [0u16; MAX_PATH + 1];
    GetModuleFileNameExW(
        process_handler,
        h_mod as _,
        exe_buf.as_mut_ptr(),
        MAX_PATH as DWORD + 1,
    );

    PathBuf::from(null_terminated_wchar_to_string(&exe_buf))
}

impl Process {
    pub(crate) fn new_from_pid(pid: Pid, now: u64) -> Option<Process> {
        unsafe {
            let process_handler = OpenProcess(PROCESS_QUERY_INFORMATION, FALSE, pid.0 as _);
            if process_handler.is_null() {
                return None;
            }
            let mut info: MaybeUninit<PROCESS_BASIC_INFORMATION> = MaybeUninit::uninit();
            if NtQueryInformationProcess(
                process_handler,
                ProcessBasicInformation,
                info.as_mut_ptr() as *mut _,
                size_of::<PROCESS_BASIC_INFORMATION>() as _,
                null_mut(),
            ) != 0
            {
                CloseHandle(process_handler);
                return None;
            }
            let info = info.assume_init();
            Some(Process::new_with_handle(
                pid,
                if info.InheritedFromUniqueProcessId as usize != 0 {
                    Some(Pid(info.InheritedFromUniqueProcessId as _))
                } else {
                    None
                },
                process_handler,
                now,
            ))
        }
    }

    pub(crate) fn new_full(
        pid: Pid,
        parent: Option<Pid>,
        memory: u64,
        virtual_memory: u64,
        name: String,
        now: u64,
    ) -> Process {
        if let Some(handle) = get_process_handler(pid) {
            let mut h_mod = null_mut();

            unsafe {
                get_h_mod(handle, &mut h_mod);
                let exe = get_exe(handle, h_mod);
                let mut root = exe.clone();
                root.pop();
                let (cmd, environ, cwd) = match get_process_params(handle) {
                    Ok(args) => args,
                    Err(_e) => {
                        sysinfo_debug!("Failed to get process parameters: {}", _e);
                        (Vec::new(), Vec::new(), PathBuf::new())
                    }
                };
                let (start_time, run_time) = get_start_and_run_time(handle, now);
                Process {
                    handle: PtrWrapper(handle),
                    name,
                    pid,
                    parent,
                    cmd,
                    environ,
                    exe,
                    cwd,
                    root,
                    status: ProcessStatus::Run,
                    memory,
                    virtual_memory,
                    cpu_usage: 0.,
                    cpu_calc_values: CPUsageCalculationValues::new(),
                    start_time,
                    run_time,
                    updated: true,
                    old_read_bytes: 0,
                    old_written_bytes: 0,
                    read_bytes: 0,
                    written_bytes: 0,
                }
            }
        } else {
            Process {
                handle: PtrWrapper(null_mut()),
                name,
                pid,
                parent,
                cmd: Vec::new(),
                environ: Vec::new(),
                exe: get_executable_path(pid),
                cwd: PathBuf::new(),
                root: PathBuf::new(),
                status: ProcessStatus::Run,
                memory,
                virtual_memory,
                cpu_usage: 0.,
                cpu_calc_values: CPUsageCalculationValues::new(),
                start_time: 0,
                run_time: 0,
                updated: true,
                old_read_bytes: 0,
                old_written_bytes: 0,
                read_bytes: 0,
                written_bytes: 0,
            }
        }
    }

    fn new_with_handle(
        pid: Pid,
        parent: Option<Pid>,
        process_handler: HANDLE,
        now: u64,
    ) -> Process {
        let mut h_mod = null_mut();

        unsafe {
            let name = if get_h_mod(process_handler, &mut h_mod) {
                get_process_name(process_handler, h_mod)
            } else {
                String::new()
            };

            let exe = get_exe(process_handler, h_mod);
            let mut root = exe.clone();
            root.pop();
            let (cmd, environ, cwd) = match get_process_params(process_handler) {
                Ok(args) => args,
                Err(_e) => {
                    sysinfo_debug!("Failed to get process parameters: {}", _e);
                    (Vec::new(), Vec::new(), PathBuf::new())
                }
            };
            let (start_time, run_time) = get_start_and_run_time(process_handler, now);
            Process {
                handle: PtrWrapper(process_handler),
                name,
                pid,
                parent,
                cmd,
                environ,
                exe,
                cwd,
                root,
                status: ProcessStatus::Run,
                memory: 0,
                virtual_memory: 0,
                cpu_usage: 0.,
                cpu_calc_values: CPUsageCalculationValues::new(),
                start_time,
                run_time,
                updated: true,
                old_read_bytes: 0,
                old_written_bytes: 0,
                read_bytes: 0,
                written_bytes: 0,
            }
        }
    }

    pub(crate) fn update(
        &mut self,
        refresh_kind: crate::ProcessRefreshKind,
        nb_processors: u64,
        now: u64,
    ) {
        if refresh_kind.cpu() {
            compute_cpu_usage(self, nb_processors);
        }
        if refresh_kind.disk_usage() {
            update_disk_usage(self);
        }
        self.run_time = now - self.start_time();
        self.updated = true;
    }
}

impl ProcessExt for Process {
    fn kill_with(&self, signal: Signal) -> Option<bool> {
        let _c_signal = super::system::convert_signal(signal)?;
        let mut kill = process::Command::new("taskkill.exe");
        kill.arg("/PID").arg(self.pid.to_string()).arg("/F");
        kill.creation_flags(CREATE_NO_WINDOW);
        match kill.output() {
            Ok(o) => Some(o.status.success()),
            Err(_) => Some(false),
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn cmd(&self) -> &[String] {
        &self.cmd
    }

    fn exe(&self) -> &Path {
        self.exe.as_path()
    }

    fn pid(&self) -> Pid {
        self.pid
    }

    fn environ(&self) -> &[String] {
        &self.environ
    }

    fn cwd(&self) -> &Path {
        self.cwd.as_path()
    }

    fn root(&self) -> &Path {
        self.root.as_path()
    }

    fn memory(&self) -> u64 {
        self.memory
    }

    fn virtual_memory(&self) -> u64 {
        self.virtual_memory
    }

    fn parent(&self) -> Option<Pid> {
        self.parent
    }

    fn status(&self) -> ProcessStatus {
        self.status
    }

    fn start_time(&self) -> u64 {
        self.start_time
    }

    fn run_time(&self) -> u64 {
        self.run_time
    }

    fn cpu_usage(&self) -> f32 {
        self.cpu_usage
    }

    fn disk_usage(&self) -> DiskUsage {
        DiskUsage {
            written_bytes: self.written_bytes - self.old_written_bytes,
            total_written_bytes: self.written_bytes,
            read_bytes: self.read_bytes - self.old_read_bytes,
            total_read_bytes: self.read_bytes,
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            if self.handle.is_null() {
                return;
            }
            CloseHandle(*self.handle);
        }
    }
}

unsafe fn get_start_and_run_time(handle: HANDLE, now: u64) -> (u64, u64) {
    let mut fstart: FILETIME = zeroed();
    let mut x = zeroed();

    GetProcessTimes(
        handle,
        &mut fstart as *mut FILETIME,
        &mut x as *mut FILETIME,
        &mut x as *mut FILETIME,
        &mut x as *mut FILETIME,
    );
    let tmp = super::utils::filetime_to_u64(fstart);
    // 11_644_473_600 is the number of seconds between the Windows epoch (1601-01-01) and
    // the linux epoch (1970-01-01).
    let start = tmp / 10_000_000 - 11_644_473_600;
    let run_time = check_sub(now, start);
    (start, run_time)
}

#[allow(clippy::uninit_vec)]
unsafe fn ph_query_process_variable_size(
    process_handle: HANDLE,
    process_information_class: PROCESSINFOCLASS,
) -> Option<Vec<u16>> {
    let mut return_length = MaybeUninit::<ULONG>::uninit();

    let mut status = NtQueryInformationProcess(
        process_handle,
        process_information_class,
        std::ptr::null_mut(),
        0,
        return_length.as_mut_ptr() as *mut _,
    );

    if status != STATUS_BUFFER_OVERFLOW
        && status != STATUS_BUFFER_TOO_SMALL
        && status != STATUS_INFO_LENGTH_MISMATCH
    {
        return None;
    }

    let mut return_length = return_length.assume_init();
    let buf_len = (return_length as usize) / 2;
    let mut buffer: Vec<u16> = Vec::with_capacity(buf_len + 1);
    buffer.set_len(buf_len);

    status = NtQueryInformationProcess(
        process_handle,
        process_information_class,
        buffer.as_mut_ptr() as *mut _,
        return_length,
        &mut return_length as *mut _,
    );
    if !NT_SUCCESS(status) {
        return None;
    }
    buffer.push(0);
    Some(buffer)
}

unsafe fn get_cmdline_from_buffer(buffer: *const u16) -> Vec<String> {
    // Get argc and argv from the command line
    let mut argc = MaybeUninit::<i32>::uninit();
    let argv_p = winapi::um::shellapi::CommandLineToArgvW(buffer, argc.as_mut_ptr());
    if argv_p.is_null() {
        return Vec::new();
    }
    let argc = argc.assume_init();
    let argv = std::slice::from_raw_parts(argv_p, argc as usize);

    let mut res = Vec::new();
    for arg in argv {
        let len = libc::wcslen(*arg);
        let str_slice = std::slice::from_raw_parts(*arg, len);
        res.push(String::from_utf16_lossy(str_slice));
    }

    winapi::um::winbase::LocalFree(argv_p as *mut _);

    res
}

unsafe fn get_region_size(handle: HANDLE, ptr: LPVOID) -> Result<usize, &'static str> {
    let mut meminfo = MaybeUninit::<MEMORY_BASIC_INFORMATION>::uninit();
    if VirtualQueryEx(
        handle,
        ptr,
        meminfo.as_mut_ptr() as *mut _,
        size_of::<MEMORY_BASIC_INFORMATION>(),
    ) == 0
    {
        return Err("Unable to read process memory information");
    }
    let meminfo = meminfo.assume_init();
    Ok((meminfo.RegionSize as isize - ptr.offset_from(meminfo.BaseAddress)) as usize)
}

#[allow(clippy::uninit_vec)]
unsafe fn get_process_data(
    handle: HANDLE,
    ptr: LPVOID,
    size: usize,
) -> Result<Vec<u16>, &'static str> {
    let mut buffer: Vec<u16> = Vec::with_capacity(size / 2 + 1);
    buffer.set_len(size / 2);
    if ReadProcessMemory(
        handle,
        ptr as *mut _,
        buffer.as_mut_ptr() as *mut _,
        size,
        std::ptr::null_mut(),
    ) != TRUE
    {
        return Err("Unable to read process data");
    }
    Ok(buffer)
}

trait RtlUserProcessParameters {
    fn get_cmdline(&self, handle: HANDLE) -> Result<Vec<u16>, &'static str>;
    fn get_cwd(&self, handle: HANDLE) -> Result<Vec<u16>, &'static str>;
    fn get_environ(&self, handle: HANDLE) -> Result<Vec<u16>, &'static str>;
}

macro_rules! impl_RtlUserProcessParameters {
    ($t:ty) => {
        impl RtlUserProcessParameters for $t {
            fn get_cmdline(&self, handle: HANDLE) -> Result<Vec<u16>, &'static str> {
                let ptr = self.CommandLine.Buffer;
                let size = self.CommandLine.Length;
                unsafe { get_process_data(handle, ptr as _, size as _) }
            }
            fn get_cwd(&self, handle: HANDLE) -> Result<Vec<u16>, &'static str> {
                let ptr = self.CurrentDirectory.DosPath.Buffer;
                let size = self.CurrentDirectory.DosPath.Length;
                unsafe { get_process_data(handle, ptr as _, size as _) }
            }
            fn get_environ(&self, handle: HANDLE) -> Result<Vec<u16>, &'static str> {
                let ptr = self.Environment;
                unsafe {
                    let size = get_region_size(handle, ptr as LPVOID)?;
                    get_process_data(handle, ptr as _, size as _)
                }
            }
        }
    };
}

impl_RtlUserProcessParameters!(RTL_USER_PROCESS_PARAMETERS32);
impl_RtlUserProcessParameters!(RTL_USER_PROCESS_PARAMETERS);

unsafe fn get_process_params(
    handle: HANDLE,
) -> Result<(Vec<String>, Vec<String>, PathBuf), &'static str> {
    if !cfg!(target_pointer_width = "64") {
        return Err("Non 64 bit targets are not supported");
    }

    // First check if target process is running in wow64 compatibility emulator
    let mut pwow32info = MaybeUninit::<LPVOID>::uninit();
    let result = NtQueryInformationProcess(
        handle,
        ProcessWow64Information,
        pwow32info.as_mut_ptr() as *mut _,
        size_of::<LPVOID>() as u32,
        null_mut(),
    );
    if !NT_SUCCESS(result) {
        return Err("Unable to check WOW64 information about the process");
    }
    let pwow32info = pwow32info.assume_init();

    if pwow32info.is_null() {
        // target is a 64 bit process

        let mut pbasicinfo = MaybeUninit::<PROCESS_BASIC_INFORMATION>::uninit();
        let result = NtQueryInformationProcess(
            handle,
            ProcessBasicInformation,
            pbasicinfo.as_mut_ptr() as *mut _,
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            null_mut(),
        );
        if !NT_SUCCESS(result) {
            return Err("Unable to get basic process information");
        }
        let pinfo = pbasicinfo.assume_init();

        let mut peb = MaybeUninit::<PEB>::uninit();
        if ReadProcessMemory(
            handle,
            pinfo.PebBaseAddress as *mut _,
            peb.as_mut_ptr() as *mut _,
            size_of::<PEB>() as SIZE_T,
            std::ptr::null_mut(),
        ) != TRUE
        {
            return Err("Unable to read process PEB");
        }

        let peb = peb.assume_init();

        let mut proc_params = MaybeUninit::<RTL_USER_PROCESS_PARAMETERS>::uninit();
        if ReadProcessMemory(
            handle,
            peb.ProcessParameters as *mut PRTL_USER_PROCESS_PARAMETERS as *mut _,
            proc_params.as_mut_ptr() as *mut _,
            size_of::<RTL_USER_PROCESS_PARAMETERS>() as SIZE_T,
            std::ptr::null_mut(),
        ) != TRUE
        {
            return Err("Unable to read process parameters");
        }

        let proc_params = proc_params.assume_init();
        return Ok((
            get_cmd_line(&proc_params, handle),
            get_proc_env(&proc_params, handle),
            get_cwd(&proc_params, handle),
        ));
    }
    // target is a 32 bit process in wow64 mode

    let mut peb32 = MaybeUninit::<PEB32>::uninit();
    if ReadProcessMemory(
        handle,
        pwow32info,
        peb32.as_mut_ptr() as *mut _,
        size_of::<PEB32>() as SIZE_T,
        std::ptr::null_mut(),
    ) != TRUE
    {
        return Err("Unable to read PEB32");
    }
    let peb32 = peb32.assume_init();

    let mut proc_params = MaybeUninit::<RTL_USER_PROCESS_PARAMETERS32>::uninit();
    if ReadProcessMemory(
        handle,
        peb32.ProcessParameters as *mut PRTL_USER_PROCESS_PARAMETERS32 as *mut _,
        proc_params.as_mut_ptr() as *mut _,
        size_of::<RTL_USER_PROCESS_PARAMETERS32>() as SIZE_T,
        std::ptr::null_mut(),
    ) != TRUE
    {
        return Err("Unable to read 32 bit process parameters");
    }
    let proc_params = proc_params.assume_init();
    Ok((
        get_cmd_line(&proc_params, handle),
        get_proc_env(&proc_params, handle),
        get_cwd(&proc_params, handle),
    ))
}

fn get_cwd<T: RtlUserProcessParameters>(params: &T, handle: HANDLE) -> PathBuf {
    match params.get_cwd(handle) {
        Ok(buffer) => unsafe { PathBuf::from(null_terminated_wchar_to_string(buffer.as_slice())) },
        Err(_e) => {
            sysinfo_debug!("get_cwd failed to get data: {}", _e);
            PathBuf::new()
        }
    }
}

unsafe fn null_terminated_wchar_to_string(slice: &[u16]) -> String {
    match slice.iter().position(|&x| x == 0) {
        Some(pos) => OsString::from_wide(&slice[..pos])
            .to_string_lossy()
            .into_owned(),
        None => OsString::from_wide(slice).to_string_lossy().into_owned(),
    }
}

fn get_cmd_line_old<T: RtlUserProcessParameters>(params: &T, handle: HANDLE) -> Vec<String> {
    match params.get_cmdline(handle) {
        Ok(buffer) => unsafe { get_cmdline_from_buffer(buffer.as_ptr()) },
        Err(_e) => {
            sysinfo_debug!("get_cmd_line_old failed to get data: {}", _e);
            Vec::new()
        }
    }
}

#[allow(clippy::cast_ptr_alignment)]
fn get_cmd_line_new(handle: HANDLE) -> Vec<String> {
    unsafe {
        if let Some(buffer) = ph_query_process_variable_size(handle, ProcessCommandLineInformation)
        {
            let buffer = (*(buffer.as_ptr() as *const UNICODE_STRING)).Buffer;

            get_cmdline_from_buffer(buffer)
        } else {
            vec![]
        }
    }
}

fn get_cmd_line<T: RtlUserProcessParameters>(params: &T, handle: HANDLE) -> Vec<String> {
    if *WINDOWS_8_1_OR_NEWER {
        get_cmd_line_new(handle)
    } else {
        get_cmd_line_old(params, handle)
    }
}

fn get_proc_env<T: RtlUserProcessParameters>(params: &T, handle: HANDLE) -> Vec<String> {
    match params.get_environ(handle) {
        Ok(buffer) => {
            let equals = "=".encode_utf16().next().unwrap();
            let raw_env = buffer;
            let mut result = Vec::new();
            let mut begin = 0;
            while let Some(offset) = raw_env[begin..].iter().position(|&c| c == 0) {
                let end = begin + offset;
                if raw_env[begin..end].iter().any(|&c| c == equals) {
                    result.push(
                        OsString::from_wide(&raw_env[begin..end])
                            .to_string_lossy()
                            .into_owned(),
                    );
                    begin = end + 1;
                } else {
                    break;
                }
            }
            result
        }
        Err(_e) => {
            sysinfo_debug!("get_proc_env failed to get data: {}", _e);
            Vec::new()
        }
    }
}

pub(crate) fn get_executable_path(_pid: Pid) -> PathBuf {
    /*let where_req = format!("ProcessId={}", pid);

    if let Some(ret) = run_wmi(&["process", "where", &where_req, "get", "ExecutablePath"]) {
        for line in ret.lines() {
            if line.is_empty() || line == "ExecutablePath" {
                continue
            }
            return line.to_owned();
        }
    }*/
    PathBuf::new()
}

#[inline]
fn check_sub(a: u64, b: u64) -> u64 {
    if a < b {
        a
    } else {
        a - b
    }
}

/// Before changing this function, you must consider the following:
/// https://github.com/GuillaumeGomez/sysinfo/issues/459
pub(crate) fn compute_cpu_usage(p: &mut Process, nb_processors: u64) {
    unsafe {
        let mut ftime: FILETIME = zeroed();
        let mut fsys: FILETIME = zeroed();
        let mut fuser: FILETIME = zeroed();
        let mut fglobal_idle_time: FILETIME = zeroed();
        let mut fglobal_kernel_time: FILETIME = zeroed(); // notice that it includes idle time
        let mut fglobal_user_time: FILETIME = zeroed();

        GetProcessTimes(
            *p.handle,
            &mut ftime as *mut FILETIME,
            &mut ftime as *mut FILETIME,
            &mut fsys as *mut FILETIME,
            &mut fuser as *mut FILETIME,
        );
        GetSystemTimes(
            &mut fglobal_idle_time as *mut FILETIME,
            &mut fglobal_kernel_time as *mut FILETIME,
            &mut fglobal_user_time as *mut FILETIME,
        );

        let mut sys: ULARGE_INTEGER = std::mem::zeroed();
        memcpy(
            &mut sys as *mut ULARGE_INTEGER as *mut c_void,
            &mut fsys as *mut FILETIME as *mut c_void,
            size_of::<FILETIME>(),
        );
        let mut user: ULARGE_INTEGER = std::mem::zeroed();
        memcpy(
            &mut user as *mut ULARGE_INTEGER as *mut c_void,
            &mut fuser as *mut FILETIME as *mut c_void,
            size_of::<FILETIME>(),
        );
        let mut global_kernel_time: ULARGE_INTEGER = std::mem::zeroed();
        memcpy(
            &mut global_kernel_time as *mut ULARGE_INTEGER as *mut c_void,
            &mut fglobal_kernel_time as *mut FILETIME as *mut c_void,
            size_of::<FILETIME>(),
        );
        let mut global_user_time: ULARGE_INTEGER = std::mem::zeroed();
        memcpy(
            &mut global_user_time as *mut ULARGE_INTEGER as *mut c_void,
            &mut fglobal_user_time as *mut FILETIME as *mut c_void,
            size_of::<FILETIME>(),
        );

        let sys = *sys.QuadPart();
        let user = *user.QuadPart();
        let global_kernel_time = *global_kernel_time.QuadPart();
        let global_user_time = *global_user_time.QuadPart();

        let delta_global_kernel_time =
            check_sub(global_kernel_time, p.cpu_calc_values.old_system_sys_cpu);
        let delta_global_user_time =
            check_sub(global_user_time, p.cpu_calc_values.old_system_user_cpu);
        let delta_user_time = check_sub(user, p.cpu_calc_values.old_process_user_cpu);
        let delta_sys_time = check_sub(sys, p.cpu_calc_values.old_process_sys_cpu);

        let denominator = (delta_global_user_time + delta_global_kernel_time) as f64;

        p.cpu_usage = 100.0
            * ((delta_user_time + delta_sys_time) as f64
                / if denominator == 0.0 {
                    p.cpu_usage = 0.0;
                    return;
                } else {
                    denominator
                }) as f32
            * nb_processors as f32;
        p.cpu_calc_values.old_process_user_cpu = user;
        p.cpu_calc_values.old_process_sys_cpu = sys;
        p.cpu_calc_values.old_system_user_cpu = global_user_time;
        p.cpu_calc_values.old_system_sys_cpu = global_kernel_time;
    }
}

pub(crate) fn get_handle(p: &Process) -> HANDLE {
    *p.handle
}

pub(crate) fn update_disk_usage(p: &mut Process) {
    let mut counters = MaybeUninit::<IO_COUNTERS>::uninit();

    unsafe {
        let ret = GetProcessIoCounters(*p.handle, counters.as_mut_ptr());
        if ret == 0 {
            sysinfo_debug!("GetProcessIoCounters call failed on process {}", p.pid());
        } else {
            let counters = counters.assume_init();
            p.old_read_bytes = p.read_bytes;
            p.old_written_bytes = p.written_bytes;
            p.read_bytes = counters.ReadTransferCount;
            p.written_bytes = counters.WriteTransferCount;
        }
    }
}

pub(crate) fn update_memory(p: &mut Process) {
    unsafe {
        let mut pmc: PROCESS_MEMORY_COUNTERS_EX = zeroed();
        if GetProcessMemoryInfo(
            *p.handle,
            &mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX as *mut c_void
                as *mut PROCESS_MEMORY_COUNTERS,
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as DWORD,
        ) != 0
        {
            p.memory = (pmc.WorkingSetSize as u64) / 1_000;
            p.virtual_memory = (pmc.PrivateUsage as u64) / 1_000;
        }
    }
}
