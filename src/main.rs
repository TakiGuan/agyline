//! Cometix-style native Rust status line renderer for Antigravity CLI on Windows.
//!
//! Zero external crates (`std` only) for instant startup (<10ms), zero AMSI script
//! scanning overhead, and full offline compilation.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
extern "system" {
    fn SetConsoleOutputCP(wCodePageID: u32) -> i32;
    fn SetConsoleTitleW(lp_console_title: *const u16) -> i32;
}

#[cfg(windows)]
mod win_detach {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        lp_reserved: *mut u16,
        lp_desktop: *mut u16,
        lp_title: *mut u16,
        dw_x: u32,
        dw_y: u32,
        dw_x_size: u32,
        dw_y_size: u32,
        dw_x_count_chars: u32,
        dw_y_count_chars: u32,
        dw_fill_attribute: u32,
        dw_flags: u32,
        w_show_window: u16,
        cb_reserved2: u16,
        lp_reserved2: *mut u8,
        h_std_input: *mut c_void,
        h_std_output: *mut c_void,
        h_std_error: *mut c_void,
    }

    #[repr(C)]
    struct ProcessInformation {
        h_process: *mut c_void,
        h_thread: *mut c_void,
        dw_process_id: u32,
        dw_thread_id: u32,
    }

    extern "system" {
        fn CreateProcessW(
            lp_application_name: *const u16,
            lp_command_line: *mut u16,
            lp_process_attributes: *mut c_void,
            lp_thread_attributes: *mut c_void,
            b_inherit_handles: i32,
            dw_creation_flags: u32,
            lp_environment: *mut c_void,
            lp_current_directory: *const u16,
            lp_startup_info: *const StartupInfoW,
            lp_process_information: *mut ProcessInformation,
        ) -> i32;
        fn CloseHandle(h_object: *mut c_void) -> i32;
    }

    fn quote_arg(arg: &str) -> String {
        let mut out = String::from("\"");
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    for _ in 0..backslashes * 2 + 1 {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push('"');
                }
                _ => {
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    backslashes = 0;
                    out.push(ch);
                }
            }
        }
        for _ in 0..backslashes * 2 {
            out.push('\\');
        }
        out.push('"');
        out
    }

    pub fn spawn_no_inherit(exe: &OsStr, args: &[&str]) -> bool {
        let mut cmdline = quote_arg(&exe.to_string_lossy());
        for a in args {
            cmdline.push(' ');
            cmdline.push_str(&quote_arg(a));
        }
        let mut wide_cmd: Vec<u16> = OsStr::new(&cmdline)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let wide_app: Vec<u16> = exe.encode_wide().chain(Some(0)).collect();

        let mut si: StartupInfoW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<StartupInfoW>() as u32;
        let mut pi: ProcessInformation = unsafe { std::mem::zeroed() };

        let ok = unsafe {
            CreateProcessW(
                wide_app.as_ptr(),
                wide_cmd.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0, // bInheritHandles = FALSE: prevents parent stdout/stderr pipe hang!
                DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
                ptr::null_mut(),
                ptr::null(),
                &si,
                &mut pi,
            )
        };
        if ok != 0 {
            unsafe {
                CloseHandle(pi.h_process);
                CloseHandle(pi.h_thread);
            }
            true
        } else {
            false
        }
    }
}

#[cfg(windows)]
mod win_reap {
    use std::collections::HashSet;
    use std::ffi::c_void;
    use std::path::Path;

    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const INVALID_HANDLE: *mut c_void = -1isize as *mut c_void;

    #[repr(C)]
    pub(crate) struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    #[repr(C)]
    struct FileTime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }

    extern "system" {
        fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> *mut c_void;
        fn Process32FirstW(h_snapshot: *mut c_void, lppe: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(h_snapshot: *mut c_void, lppe: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(
            dw_desired_access: u32,
            b_inherit_handle: i32,
            dw_process_id: u32,
        ) -> *mut c_void;
        fn QueryFullProcessImageNameW(
            h_process: *mut c_void,
            dw_flags: u32,
            lp_exe_name: *mut u16,
            lpdw_size: *mut u32,
        ) -> i32;
        fn GetProcessTimes(
            h_process: *mut c_void,
            lp_creation_time: *mut FileTime,
            lp_exit_time: *mut FileTime,
            lp_kernel_time: *mut FileTime,
            lp_user_time: *mut FileTime,
        ) -> i32;
        fn GetSystemTimeAsFileTime(lp_system_time_as_file_time: *mut FileTime);
        fn TerminateProcess(h_process: *mut c_void, u_exit_code: u32) -> i32;
        fn CloseHandle(h_object: *mut c_void) -> i32;
    }

    struct HandleGuard(*mut c_void);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    fn wide_z_to_string(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    fn ft_to_u64(ft: &FileTime) -> u64 {
        ((ft.dw_high_date_time as u64) << 32) | (ft.dw_low_date_time as u64)
    }

    pub(crate) fn normalize_win_path(p: &str) -> String {
        let trimmed = p.trim();
        let stripped = trimmed
            .strip_prefix(r"\\?\")
            .or_else(|| trimmed.strip_prefix(r"\??\"))
            .unwrap_or(trimmed);
        stripped.replace('/', "\\").to_ascii_lowercase()
    }

    pub(crate) fn is_self_or_old_backup(candidate_norm: &str, self_norm: &str) -> bool {
        if candidate_norm == self_norm {
            return true;
        }
        candidate_norm
            .strip_prefix(self_norm)
            .map(|rest| rest.starts_with(".old.") && rest.len() > 5)
            .unwrap_or(false)
    }

    fn cleanup_unlocked_old_backups(self_exe: &Path) {
        let Some(parent) = self_exe.parent() else {
            return;
        };
        let Some(self_name) = self_exe.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        let old_prefix = format!("{}.old.", self_name.to_ascii_lowercase());
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with(&old_prefix) && name.len() > old_prefix.len() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    fn is_true_parent_alive(parent_pid: u32, child_create_100ns: u64, live_pids: &HashSet<u32>) -> bool {
        if !live_pids.contains(&parent_pid) {
            return false;
        }
        let h_parent = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, parent_pid) };
        if h_parent.is_null() {
            // Cannot inspect parent creation time; fail safe and treat as alive.
            return true;
        }
        let _guard = HandleGuard(h_parent);
        let mut c_ft = FileTime { dw_low_date_time: 0, dw_high_date_time: 0 };
        let mut e_ft = FileTime { dw_low_date_time: 0, dw_high_date_time: 0 };
        let mut k_ft = FileTime { dw_low_date_time: 0, dw_high_date_time: 0 };
        let mut u_ft = FileTime { dw_low_date_time: 0, dw_high_date_time: 0 };
        if unsafe { GetProcessTimes(h_parent, &mut c_ft, &mut e_ft, &mut k_ft, &mut u_ft) } == 0 {
            return true;
        }
        // If the current process holding `parent_pid` started AFTER the child, Windows recycled the PID.
        ft_to_u64(&c_ft) <= child_create_100ns
    }

    /// Reaps orphaned, never-executed (`0ms` CPU, single-thread) instances of the exact
    /// same `statusline.exe` binary (and any hot-swap `.old.*` backups in the same folder)
    /// left suspended when `cmd /c` is killed mid-spawn.
    pub fn reap_orphaned_suspended_self(min_age_secs: u64) -> usize {
        let Ok(self_exe) = std::env::current_exe() else {
            return 0;
        };
        let Some(self_name) = self_exe.file_name().and_then(|n| n.to_str()) else {
            return 0;
        };
        let self_name_low = self_name.to_ascii_lowercase();
        let self_old_prefix = format!("{self_name_low}.old.");
        let self_path_norm = normalize_win_path(&self_exe.to_string_lossy());
        let self_pid = std::process::id();

        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap.is_null() || snap == INVALID_HANDLE {
            return 0;
        }
        let _snap_guard = HandleGuard(snap);

        let mut live_pids: HashSet<u32> = HashSet::new();
        let mut candidates: Vec<(u32, u32)> = Vec::new(); // (pid, parent_pid)

        let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
        entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;

        if unsafe { Process32FirstW(snap, &mut entry) } == 0 {
            return 0;
        }
        loop {
            live_pids.insert(entry.th32_process_id);
            // Guard 1-3: exact exe filename (or `.old.*` backup), not self, single thread.
            if entry.th32_process_id != self_pid && entry.cnt_threads == 1 {
                let exe_name_low = wide_z_to_string(&entry.sz_exe_file).to_ascii_lowercase();
                if exe_name_low == self_name_low || exe_name_low.starts_with(&self_old_prefix) {
                    candidates.push((entry.th32_process_id, entry.th32_parent_process_id));
                }
            }
            if unsafe { Process32NextW(snap, &mut entry) } == 0 {
                break;
            }
        }

        if candidates.is_empty() {
            cleanup_unlocked_old_backups(&self_exe);
            return 0;
        }

        let mut now_ft = FileTime {
            dw_low_date_time: 0,
            dw_high_date_time: 0,
        };
        unsafe { GetSystemTimeAsFileTime(&mut now_ft) };
        let now_100ns = ft_to_u64(&now_ft);
        let min_age_100ns = min_age_secs.saturating_mul(10_000_000);

        let mut reaped = 0usize;
        for (pid, parent_pid) in candidates {
            let h = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                    0,
                    pid,
                )
            };
            if h.is_null() {
                continue;
            }
            let _h_guard = HandleGuard(h);

            // Guard 4: exact full executable path match (or same-directory `.old.*` backup).
            let mut img_buf = [0u16; 1024];
            let mut img_len = img_buf.len() as u32;
            if unsafe { QueryFullProcessImageNameW(h, 0, img_buf.as_mut_ptr(), &mut img_len) } == 0
            {
                continue;
            }
            let target_path = String::from_utf16_lossy(&img_buf[..img_len as usize]);
            if !is_self_or_old_backup(&normalize_win_path(&target_path), &self_path_norm) {
                continue;
            }

            // Guard 5: 0.00ms CPU time (never executed a single instruction) AND older than min_age_secs.
            let mut c_ft = FileTime {
                dw_low_date_time: 0,
                dw_high_date_time: 0,
            };
            let mut e_ft = FileTime {
                dw_low_date_time: 0,
                dw_high_date_time: 0,
            };
            let mut k_ft = FileTime {
                dw_low_date_time: 0,
                dw_high_date_time: 0,
            };
            let mut u_ft = FileTime {
                dw_low_date_time: 0,
                dw_high_date_time: 0,
            };
            if unsafe { GetProcessTimes(h, &mut c_ft, &mut e_ft, &mut k_ft, &mut u_ft) } == 0 {
                continue;
            }
            let create_100ns = ft_to_u64(&c_ft);
            let kernel_100ns = ft_to_u64(&k_ft);
            let user_100ns = ft_to_u64(&u_ft);

            // Guard 6: parent process must no longer be alive (accounting for Windows PID reuse).
            if is_true_parent_alive(parent_pid, create_100ns, &live_pids) {
                continue;
            }

            if kernel_100ns == 0
                && user_100ns == 0
                && now_100ns.saturating_sub(create_100ns) >= min_age_100ns
            {
                if unsafe { TerminateProcess(h, 0) } != 0 {
                    reaped += 1;
                }
            }
        }

        cleanup_unlocked_old_backups(&self_exe);
        reaped
    }

    pub fn list_live_agy_pids() -> Vec<u32> {
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap.is_null() || snap == INVALID_HANDLE {
            return Vec::new();
        }
        let _snap_guard = HandleGuard(snap);

        let mut pids = Vec::new();
        let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
        entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;

        if unsafe { Process32FirstW(snap, &mut entry) } == 0 {
            return pids;
        }
        loop {
            let exe_name = wide_z_to_string(&entry.sz_exe_file);
            if exe_name.eq_ignore_ascii_case("agy.exe") {
                pids.push(entry.th32_process_id);
            }
            if unsafe { Process32NextW(snap, &mut entry) } == 0 {
                break;
            }
        }
        pids
    }
}

#[cfg(windows)]
mod win_watch {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::win_reap;

    const SYNCHRONIZE: u32 = 0x0010_0000;
    const ERROR_ALREADY_EXISTS: u32 = 183;
    const WAIT_OBJECT_0: u32 = 0x0000_0000;
    const WAIT_TIMEOUT: u32 = 0x0000_0102;
    const MAXIMUM_WAIT_OBJECTS: usize = 64;
    const WATCHER_MUTEX_NAME: &str = "Local\\agyline-watcher-v1";
    const MULTI_INSTANCE_REAP_MIN_AGE_SECS: u64 = 2;

    pub struct WatcherOptions {
        pub poll_ms: u32,
        pub exit_settle_ms: u64,
        pub reap_interval_secs: u64,
        pub reap_min_age_secs: u64,
    }

    extern "system" {
        fn CreateMutexW(
            lp_mutex_attributes: *mut c_void,
            b_initial_owner: i32,
            lp_name: *const u16,
        ) -> *mut c_void;
        fn OpenMutexW(
            dw_desired_access: u32,
            b_inherit_handle: i32,
            lp_name: *const u16,
        ) -> *mut c_void;
        fn OpenProcess(
            dw_desired_access: u32,
            b_inherit_handle: i32,
            dw_process_id: u32,
        ) -> *mut c_void;
        fn WaitForMultipleObjects(
            n_count: u32,
            lp_handles: *const *mut c_void,
            b_wait_all: i32,
            dw_milliseconds: u32,
        ) -> u32;
        fn GetLastError() -> u32;
        fn CloseHandle(h_object: *mut c_void) -> i32;
    }

    struct HandleGuard(*mut c_void);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    fn to_wide_z(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    /// Fast (<0.05ms) check for whether the global single-instance watcher is already active.
    pub fn is_watcher_running() -> bool {
        let name = to_wide_z(WATCHER_MUTEX_NAME);
        let h = unsafe { OpenMutexW(SYNCHRONIZE, 0, name.as_ptr()) };
        if h.is_null() {
            false
        } else {
            unsafe {
                CloseHandle(h);
            }
            true
        }
    }

    /// Global single-instance watcher: blocks in the kernel on all live `agy.exe` handles
    /// via `WaitForMultipleObjects` (0% CPU). Whenever any `agy.exe` exits, reaps orphaned
    /// suspended copies safely; once the last `agy.exe` exits, does a final `min_age=0`
    /// sweep and exits itself.
    pub fn run_watcher(opts: WatcherOptions) {
        let name = to_wide_z(WATCHER_MUTEX_NAME);
        let mutex = unsafe { CreateMutexW(ptr::null_mut(), 0, name.as_ptr()) };
        if mutex.is_null() {
            return;
        }
        let _mutex_guard = HandleGuard(mutex);
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return;
        }

        let mut last_periodic_reap = Instant::now();

        loop {
            let pids = win_reap::list_live_agy_pids();
            let mut guards: Vec<HandleGuard> = Vec::with_capacity(pids.len().min(MAXIMUM_WAIT_OBJECTS));
            let mut raw_handles: Vec<*mut c_void> = Vec::with_capacity(pids.len().min(MAXIMUM_WAIT_OBJECTS));

            for pid in pids.into_iter().take(MAXIMUM_WAIT_OBJECTS) {
                let h = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
                if !h.is_null() {
                    raw_handles.push(h);
                    guards.push(HandleGuard(h));
                }
            }

            if raw_handles.is_empty() {
                thread::sleep(Duration::from_millis(opts.exit_settle_ms));
                let _ = win_reap::reap_orphaned_suspended_self(0);
                break;
            }

            let wait_res = unsafe {
                WaitForMultipleObjects(
                    raw_handles.len() as u32,
                    raw_handles.as_ptr(),
                    0, // Wake as soon as ANY monitored agy.exe exits.
                    opts.poll_ms,
                )
            };

            // Release process handles before sleeping/reaping so exited processes can finalize.
            drop(raw_handles);
            drop(guards);

            if wait_res >= WAIT_OBJECT_0 && (wait_res - WAIT_OBJECT_0) < MAXIMUM_WAIT_OBJECTS as u32 {
                thread::sleep(Duration::from_millis(opts.exit_settle_ms));
                if win_reap::list_live_agy_pids().is_empty() {
                    let _ = win_reap::reap_orphaned_suspended_self(0);
                    break;
                } else {
                    let _ = win_reap::reap_orphaned_suspended_self(MULTI_INSTANCE_REAP_MIN_AGE_SECS);
                    last_periodic_reap = Instant::now();
                }
            } else if wait_res == WAIT_TIMEOUT {
                if last_periodic_reap.elapsed() >= Duration::from_secs(opts.reap_interval_secs) {
                    let _ = win_reap::reap_orphaned_suspended_self(opts.reap_min_age_secs);
                    last_periodic_reap = Instant::now();
                }
            } else {
                break;
            }
        }
    }
}

const SCRIPT_VERSION: &str = "2.0.7";
const GIT_CACHE_TTL_MS: i64 = 5_000;
const GIT_CACHE_MAX_STALE_MS: i64 = 10 * 60 * 1_000;
const GIT_REFRESH_TIMEOUT_MS: u64 = 10_000;
const GIT_LOCK_STALE_SECS: u64 = 30;
const WATCH_POLL_DEFAULT_MS: u32 = 30_000;
const EXIT_SETTLE_DEFAULT_MS: u64 = 500;
const ORPHAN_REAP_INTERVAL_SECS: u64 = 60;
const ORPHAN_MIN_AGE_SECS: u64 = 15;
const STDIN_TIMEOUT_MS: u64 = 250;
const STDIN_MAX_BYTES: usize = 1024 * 1024;

const DEFAULT_CONFIG_TOML: &str = r#"# agyline configuration (optional; zero-config works out of the box)
# Place beside the executable as <exe_stem>.toml (e.g. statusline.toml) or agyline.toml.
# Precedence: CLI flags > this file > built-in defaults.

glyphs = "nerd"          # "nerd" | "classic"
title = true             # Sync conversation_title to Windows Terminal tab

[layout]
width = "auto"           # "auto" or integer column count
dir_style = "name"       # "name" | "parent" | "tilde"
quota_mode = "used"      # "used" | "remaining"

[git]
fast = false             # true = read .git/HEAD only (skip git status)
cache_ms = 5000          # SWR fresh cache TTL in ms (500..600000)
timeout_ms = 10000       # Background git status timeout in ms (500..60000)

[watcher]
# Windows single-instance agy.exe exit watcher & orphan reaper
enabled = true
poll_ms = 30000          # Rescan interval for newly opened agy.exe (1000..300000)
exit_settle_ms = 500     # Wait after agy exit before reaping (100..5000)
reap_interval_secs = 60  # Periodic orphan reap interval (10..3600)
reap_min_age_secs = 15   # Min age for periodic orphan reap (1..3600)

[segments]
model = true
dir = true
git = true
context = true
cache = true
mode = true
vim = true
subagents = true
tasks = true
artifacts = true
quota = true
cost = true
"#;

static GIT_REFRESH_SPAWNED: AtomicBool = AtomicBool::new(false);

// --- Configuration & CLI Arguments -------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionMode {
    Render,
    Version,
    Legend,
    Preview,
    InitConfig,
    RefreshGit,
    ReapOrphans,
    WatchAgy,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuotaMode {
    Used,
    Remaining,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirStyle {
    Name,
    Parent,
    Tilde,
}

struct Config {
    use_classic: bool,
    fast_git: bool,
    show_model: bool,
    show_dir: bool,
    show_git: bool,
    show_context: bool,
    show_cache: bool,
    show_mode: bool,
    show_vim: bool,
    show_subagents: bool,
    show_tasks: bool,
    show_artifacts: bool,
    show_quota: bool,
    show_cost: bool,
    set_title: bool,
    watch_agy: bool,
    git_cache_ms: i64,
    git_timeout_ms: u64,
    watch_poll_ms: u32,
    exit_settle_ms: u64,
    reap_interval_secs: u64,
    reap_min_age_secs: u64,
    quota_mode: QuotaMode,
    dir_style: DirStyle,
    width_override: usize,
    action_mode: ActionMode,
    refresh_git_dir: String,
    reap_min_age: Option<u64>,
    no_color: bool,
}

impl Default for Config {
    fn default() -> Self {
        let no_color = env::var("NO_COLOR").map(|v| !v.is_empty()).unwrap_or(false);
        Self {
            use_classic: false,
            fast_git: false,
            show_model: true,
            show_dir: true,
            show_git: true,
            show_context: true,
            show_cache: true,
            show_mode: true,
            show_vim: true,
            show_subagents: true,
            show_tasks: true,
            show_artifacts: true,
            show_quota: true,
            show_cost: true,
            set_title: true,
            watch_agy: true,
            git_cache_ms: GIT_CACHE_TTL_MS,
            git_timeout_ms: GIT_REFRESH_TIMEOUT_MS,
            watch_poll_ms: WATCH_POLL_DEFAULT_MS,
            exit_settle_ms: EXIT_SETTLE_DEFAULT_MS,
            reap_interval_secs: ORPHAN_REAP_INTERVAL_SECS,
            reap_min_age_secs: ORPHAN_MIN_AGE_SECS,
            quota_mode: QuotaMode::Used,
            dir_style: DirStyle::Name,
            width_override: 0,
            action_mode: ActionMode::Render,
            refresh_git_dir: String::new(),
            reap_min_age: None,
            no_color,
        }
    }
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if in_double {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
        } else if in_single {
            if ch == '\'' {
                in_single = false;
            }
        } else {
            match ch {
                '"' => in_double = true,
                '\'' => in_single = true,
                '#' => return &line[..idx],
                _ => {}
            }
        }
    }
    line
}

fn parse_toml_string(val: &str) -> Option<String> {
    let t = val.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let mut out = String::new();
        let mut chars = t[1..t.len() - 1].chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(esc) = chars.next() {
                    match esc {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        '\\' => out.push('\\'),
                        '"' => out.push('"'),
                        other => out.push(other),
                    }
                }
            } else {
                out.push(c);
            }
        }
        Some(out)
    } else if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        Some(t[1..t.len() - 1].to_string())
    } else {
        None
    }
}

fn parse_toml_bool(val: &str) -> Option<bool> {
    match val.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_toml_u64(val: &str) -> Option<u64> {
    val.trim().replace('_', "").parse::<u64>().ok()
}

fn find_sidecar_config_path() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let dir = exe.parent()?;
    if let Some(stem) = exe.file_stem().and_then(|s| s.to_str()) {
        let stem_cfg = dir.join(format!("{stem}.toml"));
        if stem_cfg.is_file() {
            return Some(stem_cfg);
        }
    }
    let default_cfg = dir.join("agyline.toml");
    if default_cfg.is_file() {
        return Some(default_cfg);
    }
    None
}

fn target_init_config_path() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let dir = exe.parent()?;
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("agyline");
    Some(dir.join(format!("{stem}.toml")))
}

impl Config {
    fn apply_toml(&mut self, content: &str) {
        let clean = content.strip_prefix('\u{FEFF}').unwrap_or(content);
        let mut section = String::new();
        for raw_line in clean.lines() {
            let line = strip_toml_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') && !line.starts_with("[[") {
                section = line[1..line.len() - 1].trim().to_ascii_lowercase();
                continue;
            }
            let Some((k_raw, v_raw)) = line.split_once('=') else {
                continue;
            };
            let key = k_raw.trim().to_ascii_lowercase();
            let val = v_raw.trim();

            match (section.as_str(), key.as_str()) {
                ("", "glyphs") => {
                    if let Some(s) = parse_toml_string(val) {
                        match s.trim().to_ascii_lowercase().as_str() {
                            "classic" | "unicode" | "plain" => self.use_classic = true,
                            "nerd" | "nerdfont" => self.use_classic = false,
                            _ => {}
                        }
                    }
                }
                ("", "title") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.set_title = b;
                    }
                }
                ("layout", "width") => {
                    if let Some(s) = parse_toml_string(val) {
                        if s.trim().eq_ignore_ascii_case("auto") {
                            self.width_override = 0;
                        }
                    } else if let Some(n) = parse_toml_u64(val) {
                        self.width_override = n as usize;
                    }
                }
                ("layout", "dir_style") => {
                    if let Some(s) = parse_toml_string(val) {
                        match s.trim().to_ascii_lowercase().as_str() {
                            "name" => self.dir_style = DirStyle::Name,
                            "parent" => self.dir_style = DirStyle::Parent,
                            "tilde" => self.dir_style = DirStyle::Tilde,
                            _ => {}
                        }
                    }
                }
                ("layout", "quota_mode") => {
                    if let Some(s) = parse_toml_string(val) {
                        match s.trim().to_ascii_lowercase().as_str() {
                            "used" => self.quota_mode = QuotaMode::Used,
                            "remaining" => self.quota_mode = QuotaMode::Remaining,
                            _ => {}
                        }
                    }
                }
                ("git", "fast") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.fast_git = b;
                    }
                }
                ("git", "cache_ms") => {
                    if let Some(n) = parse_toml_u64(val) {
                        self.git_cache_ms = n.clamp(500, 600_000) as i64;
                    }
                }
                ("git", "timeout_ms") => {
                    if let Some(n) = parse_toml_u64(val) {
                        self.git_timeout_ms = n.clamp(500, 60_000);
                    }
                }
                ("watcher", "enabled") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.watch_agy = b;
                    }
                }
                ("watcher", "poll_ms") => {
                    if let Some(n) = parse_toml_u64(val) {
                        self.watch_poll_ms = n.clamp(1_000, 300_000) as u32;
                    }
                }
                ("watcher", "exit_settle_ms") => {
                    if let Some(n) = parse_toml_u64(val) {
                        self.exit_settle_ms = n.clamp(100, 5_000);
                    }
                }
                ("watcher", "reap_interval_secs") => {
                    if let Some(n) = parse_toml_u64(val) {
                        self.reap_interval_secs = n.clamp(10, 3_600);
                    }
                }
                ("watcher", "reap_min_age_secs") => {
                    if let Some(n) = parse_toml_u64(val) {
                        self.reap_min_age_secs = n.clamp(1, 3_600);
                    }
                }
                ("segments", "model") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_model = b;
                    }
                }
                ("segments", "dir") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_dir = b;
                    }
                }
                ("segments", "git") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_git = b;
                    }
                }
                ("segments", "context") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_context = b;
                    }
                }
                ("segments", "cache") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_cache = b;
                    }
                }
                ("segments", "mode") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_mode = b;
                    }
                }
                ("segments", "vim") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_vim = b;
                    }
                }
                ("segments", "subagents") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_subagents = b;
                    }
                }
                ("segments", "tasks") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_tasks = b;
                    }
                }
                ("segments", "artifacts") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_artifacts = b;
                    }
                }
                ("segments", "quota") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_quota = b;
                    }
                }
                ("segments", "cost") => {
                    if let Some(b) = parse_toml_bool(val) {
                        self.show_cost = b;
                    }
                }
                _ => {}
            }
        }
    }

    fn load_with_args<I: IntoIterator<Item = String>>(iter: I) -> Self {
        let mut cfg = Self::default();
        if let Some(path) = find_sidecar_config_path() {
            if let Ok(raw) = fs::read_to_string(path) {
                cfg.apply_toml(&raw);
            }
        }
        cfg.apply_args(iter);
        cfg
    }

    #[cfg(test)]
    fn parse_args<I: IntoIterator<Item = String>>(iter: I) -> Self {
        let mut cfg = Self::default();
        cfg.apply_args(iter);
        cfg
    }

    fn apply_args<I: IntoIterator<Item = String>>(&mut self, iter: I) {
        let args: Vec<String> = iter.into_iter().collect();
        let mut idx = 0;
        while idx < args.len() {
            let a = args[idx].to_ascii_lowercase();
            match a.as_str() {
                "--version" | "-version" | "-v" => self.action_mode = ActionMode::Version,
                "--legend" | "-legend" | "-l" => self.action_mode = ActionMode::Legend,
                "--preview" | "-preview" => self.action_mode = ActionMode::Preview,
                "--init-config" | "-initconfig" => self.action_mode = ActionMode::InitConfig,
                "--refresh-git" | "refresh-git" => {
                    self.action_mode = ActionMode::RefreshGit;
                    if idx + 1 < args.len() {
                        self.refresh_git_dir = args[idx + 1].clone();
                        idx += 1;
                    }
                }
                "--reap-orphans" | "reap-orphans" => self.action_mode = ActionMode::ReapOrphans,
                "--watch-agy" | "watch-agy" => self.action_mode = ActionMode::WatchAgy,
                "--min-age" | "-minage" => {
                    if idx + 1 < args.len() {
                        if let Ok(s) = args[idx + 1].trim().parse::<u64>() {
                            self.reap_min_age = Some(s);
                        }
                        idx += 1;
                    }
                }
                "--width" | "-width" => {
                    if idx + 1 < args.len() {
                        if let Ok(w) = args[idx + 1].trim().parse::<usize>() {
                            if w > 0 {
                                self.width_override = w;
                            }
                        }
                        idx += 1;
                    }
                }
                "--dir-style" | "-dirstyle" => {
                    if idx + 1 < args.len() {
                        match args[idx + 1].trim().to_ascii_lowercase().as_str() {
                            "name" => self.dir_style = DirStyle::Name,
                            "parent" => self.dir_style = DirStyle::Parent,
                            "tilde" => self.dir_style = DirStyle::Tilde,
                            _ => {}
                        }
                        idx += 1;
                    }
                }
                "--quota-mode" | "-quotamode" => {
                    if idx + 1 < args.len() {
                        match args[idx + 1].trim().to_ascii_lowercase().as_str() {
                            "used" => self.quota_mode = QuotaMode::Used,
                            "remaining" => self.quota_mode = QuotaMode::Remaining,
                            _ => {}
                        }
                        idx += 1;
                    }
                }
                "--classic"
                | "-classic"
                | "-c"
                | "--no-nerdfont"
                | "-nonerdfont"
                | "--compatibility"
                | "-compatibility" => self.use_classic = true,
                "--fast-git" | "-fastgit" => self.fast_git = true,
                "--no-model" | "-nomodel" => self.show_model = false,
                "--no-dir" | "--no-cwd" | "-nodir" | "-nocwd" => self.show_dir = false,
                "--no-git" | "--no-branch" | "-nogit" | "-nobranch" => self.show_git = false,
                "--no-context" | "--no-context-usage" | "-nocontext" | "-nocontextusage" => {
                    self.show_context = false
                }
                "--no-cache" | "-nocache" => self.show_cache = false,
                "--no-mode" | "-nomode" => self.show_mode = false,
                "--no-vim" | "--no-vim-mode" | "-novim" | "-novimmode" => self.show_vim = false,
                "--no-subagents" | "-nosubagents" => self.show_subagents = false,
                "--no-tasks" | "-notasks" => self.show_tasks = false,
                "--no-artifacts" | "-noartifacts" => self.show_artifacts = false,
                "--no-activity" | "-noactivity" => {
                    self.show_subagents = false;
                    self.show_tasks = false;
                    self.show_artifacts = false;
                }
                "--no-quota" | "-noquota" => self.show_quota = false,
                "--no-cost" | "-nocost" => self.show_cost = false,
                "--no-title" | "-notitle" => self.set_title = false,
                "--no-watch" | "-nowatch" => self.watch_agy = false,
                _ => {}
            }
            idx += 1;
        }
    }
}

// --- Palette & Glyphs (Cometix terminal preset) ------------------------------

struct Theme {
    c_reset: &'static str,
    c_crit: &'static str,
    c_ok: &'static str,
    c_warn: &'static str,
    c_cache: &'static str,
    c_quota: &'static str,
    c_sep: &'static str,
    c_muted: &'static str,
    c_dir: &'static str,
    c_dir_icon: &'static str,
    c_activity: &'static str,
    c_git: &'static str,
    c_context: &'static str,
    c_model: &'static str,
    c_cost: &'static str,
    ic_model: &'static str,
    ic_dir: &'static str,
    ic_git: &'static str,
    ic_context: &'static str,
    ic_cache: &'static str,
    ic_quota: &'static str,
    ic_cost: &'static str,
    ic_subagents: &'static str,
    ic_tasks: &'static str,
    ic_artifacts: &'static str,
    ic_clean: &'static str,
    ic_dirty: &'static str,
    ic_conflict: &'static str,
    ic_up: &'static str,
    ic_down: &'static str,
    mid_dot: &'static str,
    sep_str: String,
}

impl Theme {
    fn new(use_classic: bool, no_color: bool) -> Self {
        let a = |s: &'static str| if no_color { "" } else { s };
        let c_reset = a("\x1b[0m");
        let c_warn = a("\x1b[33m");
        let c_sep = a("\x1b[37m");
        let sep_str = format!("{c_sep} | {c_reset}");

        let (
            ic_model,
            ic_dir,
            ic_git,
            ic_context,
            ic_cache,
            ic_quota,
            ic_cost,
            ic_subagents,
            ic_tasks,
            ic_artifacts,
        ) = if use_classic {
            (
                "\u{25C6}", "\u{25A3}", "\u{2387}", "\u{25D4}", "\u{2248}", "\u{25F7}", "", "@",
                "*", "#",
            )
        } else {
            (
                "\u{F0629}",
                "\u{F024B}",
                "\u{F02A2}",
                "\u{F49B}",
                "\u{F1C0}",
                "\u{F0520}",
                "\u{F01C1}",
                "\u{F167A}",
                "\u{F0AE}",
                "\u{F0F6}",
            )
        };

        Self {
            c_reset,
            c_crit: a("\x1b[31m"),
            c_ok: a("\x1b[32m"),
            c_warn,
            c_cache: a("\x1b[34m"),
            c_quota: a("\x1b[36m"),
            c_sep,
            c_muted: a("\x1b[38;2;168;181;200m"),
            c_dir: a("\x1b[92m"),
            c_dir_icon: a("\x1b[93m"),
            c_activity: a("\x1b[93m"),
            c_git: a("\x1b[94m"),
            c_context: a("\x1b[95m"),
            c_model: a("\x1b[96m"),
            c_cost: c_warn,
            ic_model,
            ic_dir,
            ic_git,
            ic_context,
            ic_cache,
            ic_quota,
            ic_cost,
            ic_subagents,
            ic_tasks,
            ic_artifacts,
            ic_clean: "\u{2713}",
            ic_dirty: "\u{25CF}",
            ic_conflict: "\u{26A0}",
            ic_up: "\u{2191}",
            ic_down: "\u{2193}",
            mid_dot: "\u{00B7}",
            sep_str,
        }
    }
}

// --- Zero-Dependency RFC 8259 JSON Parser ------------------------------------

#[derive(Debug, Clone)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    fn get(&self, key: &str) -> Option<&JsonValue> {
        if let JsonValue::Object(entries) = self {
            for (k, v) in entries {
                if k == key {
                    return Some(v);
                }
            }
        }
        None
    }

    #[allow(dead_code)]
    fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(b) => Some(*b),
            JsonValue::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Number(n) if n.is_finite() => Some(*n),
            JsonValue::String(s) => s.trim().replace(',', ".").parse::<f64>().ok().filter(|n| n.is_finite()),
            _ => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Number(n) if n.is_finite() => Some(n.round() as i64),
            JsonValue::String(s) => {
                let t = s.trim();
                if let Ok(v) = t.parse::<i64>() {
                    return Some(v);
                }
                t.replace(',', ".")
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite())
                    .map(|n| n.round() as i64)
            }
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }

    fn as_object_entries(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            JsonValue::Object(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    fn to_display_string(&self) -> Option<String> {
        match self {
            JsonValue::Null => None,
            JsonValue::String(s) => Some(s.clone()),
            JsonValue::Bool(b) => Some(b.to_string()),
            JsonValue::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> JsonParser<'a> {
    fn parse(input: &'a str) -> Result<JsonValue, &'static str> {
        let trimmed = input.strip_prefix('\u{FEFF}').unwrap_or(input);
        let mut p = Self {
            bytes: trimmed.as_bytes(),
            pos: 0,
            depth: 0,
        };
        p.skip_ws();
        let val = p.parse_value()?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return Err("trailing characters");
        }
        Ok(val)
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_value(&mut self) -> Result<JsonValue, &'static str> {
        self.skip_ws();
        let b = self.peek().ok_or("unexpected eof")?;
        match b {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(JsonValue::String),
            b't' => self.parse_literal(b"true", JsonValue::Bool(true)),
            b'f' => self.parse_literal(b"false", JsonValue::Bool(false)),
            b'n' => self.parse_literal(b"null", JsonValue::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err("invalid token"),
        }
    }

    fn parse_literal(&mut self, lit: &[u8], val: JsonValue) -> Result<JsonValue, &'static str> {
        if self.bytes.len().saturating_sub(self.pos) >= lit.len()
            && &self.bytes[self.pos..self.pos + lit.len()] == lit
        {
            self.pos += lit.len();
            Ok(val)
        } else {
            Err("invalid literal")
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, &'static str> {
        self.depth += 1;
        if self.depth > 64 {
            return Err("recursion limit");
        }
        self.pos += 1; // '{'
        self.skip_ws();
        let mut entries = Vec::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonValue::Object(entries));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err("expected string key");
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err("expected colon");
            }
            self.pos += 1;
            let val = self.parse_value()?;
            entries.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("expected comma or closing brace"),
            }
        }
        self.depth -= 1;
        Ok(JsonValue::Object(entries))
    }

    fn parse_array(&mut self) -> Result<JsonValue, &'static str> {
        self.depth += 1;
        if self.depth > 64 {
            return Err("recursion limit");
        }
        self.pos += 1; // '['
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            let val = self.parse_value()?;
            items.push(val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("expected comma or closing bracket"),
            }
        }
        self.depth -= 1;
        Ok(JsonValue::Array(items))
    }

    fn parse_hex4(&mut self) -> Result<u16, &'static str> {
        if self.bytes.len().saturating_sub(self.pos) < 4 {
            return Err("truncated unicode escape");
        }
        let s = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| "invalid utf8 in escape")?;
        let v = u16::from_str_radix(s, 16).map_err(|_| "invalid hex escape")?;
        self.pos += 4;
        Ok(v)
    }

    fn parse_string(&mut self) -> Result<String, &'static str> {
        self.pos += 1; // opening '"'
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if b == b'\\' {
                self.pos += 1;
                let esc = self.peek().ok_or("unterminated escape")?;
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let u1 = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&u1) {
                            if self.bytes.len().saturating_sub(self.pos) >= 2
                                && self.bytes[self.pos] == b'\\'
                                && self.bytes[self.pos + 1] == b'u'
                            {
                                self.pos += 2;
                                let u2 = self.parse_hex4()?;
                                if (0xDC00..=0xDFFF).contains(&u2) {
                                    let cp = 0x10000
                                        + (((u1 as u32 - 0xD800) << 10) | (u2 as u32 - 0xDC00));
                                    if let Some(ch) = char::from_u32(cp) {
                                        out.push(ch);
                                    }
                                }
                            }
                        } else if let Some(ch) = char::from_u32(u1 as u32) {
                            out.push(ch);
                        }
                    }
                    _ => return Err("unknown escape"),
                }
            } else if b < 0x20 {
                return Err("unescaped control char");
            } else if b < 0x80 {
                out.push(b as char);
                self.pos += 1;
            } else {
                let rem = std::str::from_utf8(&self.bytes[self.pos..])
                    .map_err(|_| "invalid utf8 in string")?;
                let ch = rem.chars().next().ok_or("eof in utf8")?;
                out.push(ch);
                self.pos += ch.len_utf8();
            }
        }
        Err("unterminated string")
    }

    fn parse_number(&mut self) -> Result<JsonValue, &'static str> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err("invalid number"),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err("invalid fraction");
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err("invalid exponent");
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| "invalid utf8")?;
        let n = s.parse::<f64>().map_err(|_| "float parse error")?;
        Ok(JsonValue::Number(n))
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// --- Pure-Rust SHA-256 (for cache filename parity with PowerShell) -----------

fn sha256_hex_16(input: &str) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let data = input.as_bytes();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    format!("{:08x}{:08x}", h[0], h[1])
}

// --- String Sanitization, Display Width & Formatting Helpers -----------------

fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1B {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'[' => {
                    // CSI: ESC [ [0-9;?]* [a-zA-Z]
                    i += 1;
                    while i < bytes.len()
                        && (bytes[i].is_ascii_digit() || bytes[i] == b';' || bytes[i] == b'?')
                    {
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                }
                b']' => {
                    // OSC: ESC ] ... (BEL | ESC \)
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                b'(' => {
                    i += 1;
                    if i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                }
                _ => {}
            }
        } else {
            let rem = &input[i..];
            let ch = rem.chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn sanitize_string(input: &str) -> String {
    let no_ansi = strip_ansi(input);
    let mut out = String::with_capacity(no_ansi.len());
    for ch in no_ansi.chars() {
        let cp = ch as u32;
        if cp <= 0x1F || cp == 0x7F {
            continue;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

fn sanitize_json_str(val: Option<&JsonValue>) -> String {
    match val.and_then(|v| v.to_display_string()) {
        Some(s) => sanitize_string(&s),
        None => String::new(),
    }
}

fn display_width(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let clean = strip_ansi(text);
    let mut width = 0usize;
    for ch in clean.chars() {
        let cp = ch as u32;
        if (0x0300..=0x036F).contains(&cp)
            || (0xFE00..=0xFE0F).contains(&cp)
            || (0x200B..=0x200D).contains(&cp)
            || cp == 0xFEFF
        {
            continue;
        }
        // Nerd Font PUA glyphs occupy 1 terminal cell.
        if (0xE000..=0xF8FF).contains(&cp)
            || (0xF0000..=0xFFFFD).contains(&cp)
            || (0x100000..=0x10FFFD).contains(&cp)
        {
            width += 1;
            continue;
        }
        // East Asian Wide / CJK ranges occupy 2 terminal cells.
        if (0x1100..=0x115F).contains(&cp)
            || (0x2E80..=0x9FFF).contains(&cp)
            || (0xAC00..=0xD7A3).contains(&cp)
            || (0xF900..=0xFAFF).contains(&cp)
            || (0xFE10..=0xFE6F).contains(&cp)
            || (0xFF00..=0xFF60).contains(&cp)
            || (0xFFE0..=0xFFE6).contains(&cp)
            || (0x20000..=0x2FFFD).contains(&cp)
            || (0x30000..=0x3FFFD).contains(&cp)
        {
            width += 2;
            continue;
        }
        width += 1;
    }
    width
}

fn trim_trailing_zeros(s: String) -> String {
    if !s.contains('.') {
        return s;
    }
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_string()
}

fn format_tokens(count: i64) -> String {
    if count >= 1_000_000 {
        let v = count as f64 / 1_000_000.0;
        if v >= 10.0 {
            return format!("{}M", trim_trailing_zeros(format!("{:.1}", (v * 10.0).round() / 10.0)));
        }
        return format!("{}M", trim_trailing_zeros(format!("{:.2}", (v * 100.0).round() / 100.0)));
    }
    if count >= 1_000 {
        let v = count as f64 / 1_000.0;
        if v >= 10.0 {
            return format!("{}k", v.round() as i64);
        }
        return format!("{}k", trim_trailing_zeros(format!("{:.1}", (v * 10.0).round() / 10.0)));
    }
    count.to_string()
}

fn format_percent(pct: f64) -> String {
    let clamped = pct.clamp(0.0, 100.0);
    format!("{}", clamped.round() as i64)
}

fn format_usd(val: f64) -> String {
    if val <= 0.0 || !val.is_finite() {
        return String::new();
    }
    if val < 0.0001 {
        return "<$0.0001".to_string();
    }
    if val < 0.00995 {
        let r4 = (val * 10_000.0).round() / 10_000.0;
        return format!("${:.4}", r4);
    }
    let r2 = (val * 100.0).round() / 100.0;
    format!("${:.2}", r2)
}

fn format_countdown(seconds: i64) -> String {
    if seconds <= 0 {
        return String::new();
    }
    if seconds < 60 {
        return "<1m".to_string();
    }
    let days = seconds / 86_400;
    let rem = seconds % 86_400;
    let hours = rem / 3_600;
    let mins = (rem % 3_600) / 60;
    if days > 0 {
        if hours > 0 {
            return format!("{days}d{hours}h");
        }
        return format!("{days}d");
    }
    if hours > 0 {
        if mins > 0 {
            return format!("{hours}h{mins}m");
        }
        return format!("{hours}h");
    }
    format!("{mins}m")
}

fn format_effort_label(effort: &str) -> String {
    let t = effort.trim();
    if t.is_empty() {
        return String::new();
    }
    match t.to_ascii_lowercase().as_str() {
        "low" => "Low".to_string(),
        "med" | "medium" => "Med".to_string(),
        "high" => "High".to_string(),
        "xhigh" => "xHigh".to_string(),
        "max" => "Max".to_string(),
        _ => {
            let mut chars = t.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_unix_sec() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parses ISO 8601 UTC/offset timestamps (e.g. `2026-11-25T12:30:00Z` or `+08:00`) to Unix seconds.
fn parse_iso8601_unix_sec(s: &str) -> Option<i64> {
    let b = s.trim().as_bytes();
    if b.len() < 19 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't' && b[10] != b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let parse_i64 = |range: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(b.get(range)?).ok()?.parse::<i64>().ok()
    };
    let year = parse_i64(0..4)?;
    let month = parse_i64(5..7)?;
    let day = parse_i64(8..10)?;
    let hour = parse_i64(11..13)?;
    let min = parse_i64(14..16)?;
    let sec = parse_i64(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&min)
        || !(0..=60).contains(&sec)
    {
        return None;
    }

    // Skip optional fractional seconds.
    let mut pos = 19;
    if b.get(pos) == Some(&b'.') {
        pos += 1;
        while pos < b.len() && b[pos].is_ascii_digit() {
            pos += 1;
        }
    }

    let mut tz_offset_sec: i64 = 0;
    if pos < b.len() {
        match b[pos] {
            b'Z' | b'z' => {}
            b'+' | b'-' => {
                let sign: i64 = if b[pos] == b'+' { 1 } else { -1 };
                pos += 1;
                let oh = parse_i64(pos..pos + 2)?;
                pos += 2;
                if b.get(pos) == Some(&b':') {
                    pos += 1;
                }
                let om = if pos + 2 <= b.len() {
                    parse_i64(pos..pos + 2)?
                } else {
                    0
                };
                tz_offset_sec = sign * (oh * 3600 + om * 60);
            }
            _ => return None,
        }
    }

    // Howard Hinnant's days-from-civil algorithm (Unix epoch 1970-01-01).
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m_adj = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m_adj + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86_400 + hour * 3_600 + min * 60 + sec - tz_offset_sec)
}

// --- Fast Git Probe + File Cache (RAII temp cleanup, zero hangs) -------------

#[derive(Clone)]
struct GitInfo {
    ts: i64,
    ttl: i64,
    branch: String,
    dirty: bool,
    conflicts: i32,
    ahead: i32,
    behind: i32,
    has_status: bool,
}

struct TempFileGuard(PathBuf);
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn get_git_branch_fast(start_dir: &str) -> String {
    if start_dir.trim().is_empty() {
        return String::new();
    }
    let mut cur = PathBuf::from(start_dir);
    if !cur.is_dir() {
        return String::new();
    }
    for _ in 0..25 {
        let dot_git = cur.join(".git");
        let head_path = if dot_git.is_dir() {
            Some(dot_git.join("HEAD"))
        } else if dot_git.is_file() {
            fs::read_to_string(&dot_git).ok().and_then(|content| {
                let line = content.trim();
                let rest = line.strip_prefix("gitdir:")?.trim();
                let p = Path::new(rest);
                let resolved = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    cur.join(p)
                };
                Some(resolved.join("HEAD"))
            })
        } else {
            None
        };

        if let Some(hp) = head_path {
            if hp.is_file() {
                if let Ok(head_raw) = fs::read_to_string(&hp) {
                    let head = head_raw.trim();
                    if let Some(b) = head.strip_prefix("ref: refs/heads/") {
                        return sanitize_string(b);
                    }
                    if let Some(b) = head.strip_prefix("ref: ") {
                        return sanitize_string(b);
                    }
                    if head.len() >= 7 {
                        let short: String = head.chars().take(7).collect();
                        return format!("@{}", sanitize_string(&short));
                    }
                }
                return String::new();
            }
        }

        match cur.parent() {
            Some(p) if p != cur => cur = p.to_path_buf(),
            _ => break,
        }
    }
    String::new()
}

fn get_git_cache_paths(repo_dir: &str) -> (PathBuf, PathBuf) {
    let norm = repo_dir
        .trim()
        .trim_end_matches(|c| c == '\\' || c == '/')
        .to_lowercase();
    let short = sha256_hex_16(&norm);
    let tmp = env::temp_dir();
    (
        tmp.join(format!("agy-statusline-git-{short}.json")),
        tmp.join(format!("agy-statusline-git-{short}.lock")),
    )
}

fn try_claim_lock(lock_path: &Path, stale: Duration) -> bool {
    let create = || {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
    };
    match create() {
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let abandoned = fs::metadata(lock_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age > stale)
                .unwrap_or(false);
            abandoned && fs::remove_file(lock_path).is_ok() && create().is_ok()
        }
        Err(_) => false,
    }
}

fn spawn_detached_git_refresh(repo_dir: &str, lock_path: &Path) {
    if !try_claim_lock(lock_path, Duration::from_secs(GIT_LOCK_STALE_SECS)) {
        return;
    }
    let Ok(exe) = env::current_exe() else {
        let _ = fs::remove_file(lock_path);
        return;
    };
    #[cfg(windows)]
    let started = win_detach::spawn_no_inherit(exe.as_os_str(), &["--refresh-git", repo_dir]);
    #[cfg(not(windows))]
    let started = Command::new(exe)
        .arg("--refresh-git")
        .arg(repo_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok();
    if started {
        GIT_REFRESH_SPAWNED.store(true, Ordering::Relaxed);
    } else {
        let _ = fs::remove_file(lock_path);
    }
}

fn get_reap_paths() -> (PathBuf, PathBuf) {
    let tmp = env::temp_dir();
    (
        tmp.join("agy-statusline-reap.stamp"),
        tmp.join("agy-statusline-reap.lock"),
    )
}

fn is_reap_due(stamp_path: &Path, interval_secs: u64) -> bool {
    fs::metadata(stamp_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age >= Duration::from_secs(interval_secs))
        .unwrap_or(true)
}

fn reap_orphans_now(stamp_path: &Path, min_age_secs: u64) {
    let _ = fs::write(stamp_path, now_unix_ms().to_string().as_bytes());
    #[cfg(windows)]
    {
        let _ = win_reap::reap_orphaned_suspended_self(min_age_secs);
    }
    #[cfg(not(windows))]
    {
        let _ = min_age_secs;
    }
}

fn reap_orphans_if_due(cfg: &Config) {
    let (stamp_path, lock_path) = get_reap_paths();
    if !is_reap_due(&stamp_path, cfg.reap_interval_secs) {
        return;
    }
    if !try_claim_lock(&lock_path, Duration::from_secs(cfg.reap_interval_secs)) {
        return;
    }
    let _lock_guard = TempFileGuard(lock_path);
    reap_orphans_now(&stamp_path, cfg.reap_min_age_secs);
}

fn reap_orphans_worker(min_age_secs: u64) {
    let (stamp_path, lock_path) = get_reap_paths();
    let _lock_guard = TempFileGuard(lock_path);
    reap_orphans_now(&stamp_path, min_age_secs);
}

fn maybe_ensure_agy_watcher() {
    #[cfg(windows)]
    {
        if win_watch::is_watcher_running() {
            return;
        }
        let Ok(exe) = env::current_exe() else {
            return;
        };
        let _ = win_detach::spawn_no_inherit(exe.as_os_str(), &["--watch-agy"]);
    }
}

fn maybe_spawn_detached_reap(cfg: &Config) {
    #[cfg(windows)]
    {
        // Skip spawning a second child when `--refresh-git` was already launched this turn
        // (it piggybacks `reap_orphans_if_due()` and watcher startup in the background).
        if GIT_REFRESH_SPAWNED.load(Ordering::Relaxed) {
            return;
        }
        if cfg.watch_agy {
            // Fast path (<0.05ms): if the global single-instance watcher is active, it
            // already handles both exit-time and periodic reaping.
            if win_watch::is_watcher_running() {
                return;
            }
            maybe_ensure_agy_watcher();
            return;
        }
        let (stamp_path, lock_path) = get_reap_paths();
        // On cold start when no stamp exists yet, seed the stamp instead of adding
        // `CreateProcessW` latency to the first render; reaping will run later.
        if !stamp_path.exists() {
            let _ = fs::write(&stamp_path, now_unix_ms().to_string().as_bytes());
            return;
        }
        if !is_reap_due(&stamp_path, cfg.reap_interval_secs) {
            return;
        }
        if !try_claim_lock(&lock_path, Duration::from_secs(cfg.reap_interval_secs)) {
            return;
        }
        let Ok(exe) = env::current_exe() else {
            let _ = fs::remove_file(&lock_path);
            return;
        };
        let _ = fs::write(&stamp_path, now_unix_ms().to_string().as_bytes());
        if !win_detach::spawn_no_inherit(exe.as_os_str(), &["--reap-orphans"]) {
            let _ = fs::remove_file(&lock_path);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = cfg;
    }
}

fn refresh_git_worker(repo_dir: &str, cfg: &Config) {
    if !repo_dir.trim().is_empty() {
        let (cache_path, lock_path) = get_git_cache_paths(repo_dir);
        let _lock_guard = TempFileGuard(lock_path);
        if let Some(fresh) = query_git_status_now(repo_dir, cfg.git_timeout_ms, cfg.git_cache_ms) {
            write_git_cache_atomic(&cache_path, &fresh);
        }
    }
    reap_orphans_if_due(cfg);
    if cfg.watch_agy {
        maybe_ensure_agy_watcher();
    }
}

fn run_git_status_with_timeout(repo_dir: &str, timeout_ms: u64) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args([
        "-C",
        repo_dir,
        "--no-optional-locks",
        "-c",
        "core.quotePath=false",
        "status",
        "--porcelain=v2",
        "--branch",
        "-uno",
    ])
    .env("GIT_OPTIONAL_LOCKS", "0")
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;

    let reader = thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return reader.join().ok();
                }
                return None;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn query_git_status_now(repo_dir: &str, timeout_ms: u64, cache_ttl_ms: i64) -> Option<GitInfo> {
    if repo_dir.trim().is_empty() || !Path::new(repo_dir).is_dir() {
        return None;
    }
    let raw = run_git_status_with_timeout(repo_dir, timeout_ms)?;

    let mut branch = String::new();
    let mut oid = String::new();
    let mut ahead = 0i32;
    let mut behind = 0i32;
    let mut dirty = false;
    let mut conflicts = 0i32;

    for line in raw.lines() {
        if let Some(h) = line.strip_prefix("# branch.head ") {
            let h = h.trim();
            if h != "(detached)" {
                branch = h.to_string();
            }
        } else if let Some(o) = line.strip_prefix("# branch.oid ") {
            let o = o.trim();
            if o != "(initial)" {
                oid = o.to_string();
            }
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            let mut parts = ab.split_whitespace();
            if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                if let Some(an) = a.strip_prefix('+').and_then(|s| s.parse::<i32>().ok()) {
                    ahead = an;
                }
                if let Some(bn) = b.strip_prefix('-').and_then(|s| s.parse::<i32>().ok()) {
                    behind = bn;
                }
            }
        } else if line.starts_with("u ") {
            conflicts += 1;
            dirty = true;
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            dirty = true;
        }
    }

    if branch.is_empty() && !oid.is_empty() {
        let short_oid: String = oid.chars().take(7).collect();
        branch = format!("@{short_oid}");
    }
    if branch.is_empty() {
        return None;
    }

    Some(GitInfo {
        ts: now_unix_ms(),
        ttl: cache_ttl_ms,
        branch: sanitize_string(&branch),
        dirty,
        conflicts,
        ahead,
        behind,
        has_status: true,
    })
}

fn read_git_cache(cache_path: &Path) -> Option<GitInfo> {
    let raw = fs::read_to_string(cache_path).ok()?;
    let v = JsonParser::parse(&raw).ok()?;
    Some(GitInfo {
        ts: v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0),
        ttl: v
            .get("ttl")
            .and_then(|x| x.as_i64())
            .unwrap_or(GIT_CACHE_TTL_MS),
        branch: sanitize_json_str(v.get("branch")),
        dirty: v.get("dirty").and_then(|x| x.as_bool()).unwrap_or(false),
        conflicts: v.get("conflicts").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
        ahead: v.get("ahead").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
        behind: v.get("behind").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
        has_status: v
            .get("hasStatus")
            .and_then(|x| x.as_bool())
            .unwrap_or(true),
    })
}

fn write_git_cache_atomic(cache_path: &Path, info: &GitInfo) {
    let json = format!(
        "{{\"ts\":{},\"ttl\":{},\"branch\":{},\"dirty\":{},\"conflicts\":{},\"ahead\":{},\"behind\":{},\"hasStatus\":{}}}",
        info.ts,
        info.ttl,
        json_escape(&info.branch),
        info.dirty,
        info.conflicts,
        info.ahead,
        info.behind,
        info.has_status
    );
    let pid = std::process::id();
    let mut tmp = cache_path.as_os_str().to_os_string();
    tmp.push(format!(".{pid}.tmp"));
    let tmp_path = PathBuf::from(tmp);
    let _guard = TempFileGuard(tmp_path.clone());
    if fs::write(&tmp_path, json.as_bytes()).is_ok() {
        let _ = fs::copy(&tmp_path, cache_path);
    }
}

fn get_git_info(
    repo_dir: &str,
    vcs_obj: Option<&JsonValue>,
    fast_git: bool,
    cache_ttl_ms: i64,
) -> Option<GitInfo> {
    let payload_branch = sanitize_json_str(vcs_obj.and_then(|v| v.get("branch")));
    let payload_dirty_raw = vcs_obj.and_then(|v| v.get("dirty"));
    let payload_dirty = payload_dirty_raw.and_then(|v| v.as_bool()).unwrap_or(false);
    let payload_has_status = payload_dirty_raw.is_some();
    let vcs_type = sanitize_json_str(vcs_obj.and_then(|v| v.get("type")));

    let payload_fallback = || {
        if !payload_branch.is_empty() {
            Some(GitInfo {
                ts: 0,
                ttl: 0,
                branch: payload_branch.clone(),
                dirty: payload_dirty,
                conflicts: 0,
                ahead: 0,
                behind: 0,
                has_status: payload_has_status,
            })
        } else {
            None
        }
    };

    if !vcs_type.is_empty() && vcs_type != "git" {
        return payload_fallback();
    }
    if repo_dir.trim().is_empty() || !Path::new(repo_dir).is_dir() {
        return payload_fallback();
    }

    let fast_branch = get_git_branch_fast(repo_dir);
    if fast_branch.is_empty() {
        return payload_fallback();
    }

    if fast_git {
        return Some(GitInfo {
            ts: 0,
            ttl: 0,
            branch: fast_branch,
            dirty: payload_dirty,
            conflicts: 0,
            ahead: 0,
            behind: 0,
            has_status: payload_has_status,
        });
    }

    // Zero-block Stale-While-Revalidate (CCometixLine SWR + weby-homelab payload fast path):
    // The render path NEVER blocks waiting on `git status`.
    let (cache_path, lock_path) = get_git_cache_paths(repo_dir);
    let now_ms = now_unix_ms();
    if let Some(cached) = read_git_cache(&cache_path) {
        let age = now_ms - cached.ts;
        let effective_ttl = cache_ttl_ms.min(cached.ttl.max(500));
        if cached.branch == fast_branch && age >= 0 {
            if age <= effective_ttl {
                return Some(cached);
            }
            if age <= GIT_CACHE_MAX_STALE_MS {
                spawn_detached_git_refresh(repo_dir, &lock_path);
                return Some(cached);
            }
        }
    }

    // Cache missing, expired past max-stale, or branch switched: return instantly with
    // `.git/HEAD` branch (+ payload dirty hint if present) and refresh asynchronously.
    spawn_detached_git_refresh(repo_dir, &lock_path);
    Some(GitInfo {
        ts: now_ms,
        ttl: 0,
        branch: fast_branch,
        dirty: payload_dirty,
        conflicts: 0,
        ahead: 0,
        behind: 0,
        has_status: payload_has_status,
    })
}

// --- Bounded Stdin Reader ----------------------------------------------------

fn read_bounded_stdin(timeout_ms: u64) -> String {
    if io::stdin().is_terminal() {
        return "{}".to_string();
    }
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut acc = Vec::new();
    while Instant::now() < deadline {
        let rem = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(rem) {
            Ok(chunk) => {
                let take = chunk.len().min(STDIN_MAX_BYTES.saturating_sub(acc.len()));
                acc.extend_from_slice(&chunk[..take]);
                if let Ok(s) = std::str::from_utf8(&acc) {
                    let t = s.trim();
                    if t.starts_with('{') && t.ends_with('}') && JsonParser::parse(t).is_ok() {
                        return t.to_string();
                    }
                }
                if acc.len() >= STDIN_MAX_BYTES {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) | Err(mpsc::RecvTimeoutError::Timeout) => {
                break;
            }
        }
    }

    let final_str = String::from_utf8_lossy(&acc);
    let trimmed = final_str.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        trimmed.to_string()
    } else {
        "{}".to_string()
    }
}

// --- Segment Builders --------------------------------------------------------

struct Item {
    priority: u8,
    variants: [String; 3], // 0 = Full, 1 = Compact, 2 = Minimal
}

impl Item {
    fn new(priority: u8, full: String, compact: String, minimal: String) -> Option<Self> {
        if full.is_empty() {
            return None;
        }
        Some(Self {
            priority,
            variants: [full, compact, minimal],
        })
    }
}

fn strip_trailing_effort_paren(name: &str) -> (String, Option<String>) {
    let trimmed = name.trim_end();
    if let Some(open_idx) = trimmed.rfind('(') {
        if trimmed.ends_with(')') && open_idx < trimmed.len() - 1 {
            let inside = trimmed[open_idx + 1..trimmed.len() - 1].trim();
            let low = inside.to_ascii_lowercase();
            if matches!(low.as_str(), "low" | "med" | "medium" | "high" | "xhigh" | "max") {
                let base = trimmed[..open_idx].trim_end().to_string();
                return (base, Some(inside.to_string()));
            }
        }
    }
    (trimmed.to_string(), None)
}

fn resolve_model_icon(model_hint: &str, cfg: &Config, t: &Theme) -> &'static str {
    if cfg.use_classic {
        return t.ic_model;
    }
    if model_hint.contains("gemini") {
        return "\u{F0AE2}"; // nf-md-star_four_points
    }
    if contains_word(model_hint, "claude")
        || contains_word(model_hint, "sonnet")
        || contains_word(model_hint, "opus")
        || contains_word(model_hint, "haiku")
        || contains_word(model_hint, "anthropic")
    {
        return "\u{EC82}"; // nf-cod-claude
    }
    if contains_word(model_hint, "gpt")
        || contains_word(model_hint, "openai")
        || contains_word(model_hint, "codex")
        || contains_word(model_hint, "o1")
        || contains_word(model_hint, "o3")
        || contains_word(model_hint, "o4")
    {
        return "\u{EC81}"; // nf-cod-openai
    }
    if model_hint.contains("deepseek") {
        return "\u{F18B4}"; // nf-md-dolphin
    }
    if model_hint.contains("copilot") {
        return "\u{EC1E}"; // nf-cod-copilot
    }
    t.ic_model
}

fn build_model_item(data: &JsonValue, cfg: &Config, t: &Theme) -> Option<Item> {
    let model_obj = data.get("model");
    let display_name = sanitize_json_str(model_obj.and_then(|m| m.get("display_name")));
    let model_id = sanitize_json_str(model_obj.and_then(|m| m.get("id")));
    let model_hint = format!("{model_id} {display_name}").to_ascii_lowercase();
    let raw_name = if !display_name.is_empty() {
        display_name
    } else {
        model_id
    };
    if raw_name.is_empty() {
        return None;
    }

    let mut effort_raw = sanitize_json_str(model_obj.and_then(|m| m.get("effort")));
    if effort_raw.is_empty() {
        effort_raw = sanitize_json_str(data.get("effort").and_then(|e| e.get("level")));
    }

    let (name, paren_effort) = strip_trailing_effort_paren(&raw_name);
    if effort_raw.is_empty() {
        if let Some(pe) = paren_effort {
            effort_raw = pe;
        }
    }
    let effort = format_effort_label(&effort_raw);

    let mut vim_prefix = String::new();
    if cfg.show_vim {
        let vim_mode = sanitize_json_str(data.get("vim").and_then(|v| v.get("mode")));
        if !vim_mode.is_empty() {
            let v_short = if vim_mode == "VISUAL LINE" {
                "VL".to_string()
            } else {
                vim_mode
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_default()
            };
            vim_prefix = format!("{}[{}]{} ", t.c_muted, v_short, t.c_reset);
        }
    }

    let confirm_prefix = if data
        .get("tool_confirmation_pending")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        format!("{}?{} ", t.c_warn, t.c_reset)
    } else {
        String::new()
    };

    let model_icon = resolve_model_icon(&model_hint, cfg, t);
    let icon_part = format!("{}{} ", t.c_model, model_icon);
    let full_text = if !effort.is_empty() {
        format!(
            "{confirm_prefix}{vim_prefix}{icon_part}{name}{} {} {}{effort}{}",
            t.c_sep, t.mid_dot, t.c_model, t.c_reset
        )
    } else {
        format!("{confirm_prefix}{vim_prefix}{icon_part}{name}{}", t.c_reset)
    };

    let words: Vec<&str> = name.split_whitespace().collect();
    let short_name = if words.len() >= 2 {
        format!("{} {}", words[0], words[1])
    } else {
        name
    };
    let min_text = format!(
        "{confirm_prefix}{vim_prefix}{icon_part}{short_name}{}",
        t.c_reset
    );

    Item::new(5, full_text.clone(), full_text, min_text)
}

fn extract_dir(data: &JsonValue) -> String {
    let cur = sanitize_json_str(data.get("workspace").and_then(|w| w.get("current_dir")));
    if !cur.is_empty() {
        return cur;
    }
    sanitize_json_str(data.get("cwd"))
}

fn build_dir_item(data: &JsonValue, cfg: &Config, t: &Theme) -> Option<Item> {
    let dir = extract_dir(data);
    if dir.is_empty() {
        return None;
    }
    let parts: Vec<&str> = dir
        .split(|c| c == '\\' || c == '/')
        .filter(|p| !p.is_empty())
        .collect();
    let leaf = parts.last().copied().unwrap_or(dir.as_str());

    let shown = match cfg.dir_style {
        DirStyle::Name => leaf.to_string(),
        DirStyle::Parent => {
            if parts.len() >= 2 {
                format!("{}/{}", parts[parts.len() - 2], leaf)
            } else {
                leaf.to_string()
            }
        }
        DirStyle::Tilde => {
            let home = env::var("USERPROFILE")
                .or_else(|_| env::var("HOME"))
                .unwrap_or_default();
            if !home.is_empty()
                && dir.len() >= home.len()
                && dir[..home.len()].eq_ignore_ascii_case(&home)
            {
                format!("~{}", dir[home.len()..].replace('\\', "/"))
            } else {
                dir.replace('\\', "/")
            }
        }
    };

    let full = format!(
        "{}{} {}{shown}{}",
        t.c_dir_icon, t.ic_dir, t.c_dir, t.c_reset
    );
    let min = format!("{}{} {}{leaf}{}", t.c_dir_icon, t.ic_dir, t.c_dir, t.c_reset);
    Item::new(4, full, min.clone(), min)
}

fn build_git_item(data: &JsonValue, cfg: &Config, t: &Theme) -> Option<Item> {
    let dir = extract_dir(data);
    let info = get_git_info(&dir, data.get("vcs"), cfg.fast_git, cfg.git_cache_ms)?;
    if info.branch.is_empty() {
        return None;
    }

    let color = if info.conflicts > 0 { t.c_crit } else { t.c_git };
    let mut full_body = info.branch.clone();
    if info.has_status {
        let status_glyph = if info.conflicts > 0 {
            t.ic_conflict
        } else if info.dirty {
            t.ic_dirty
        } else {
            t.ic_clean
        };
        full_body = format!("{} {status_glyph}", info.branch);
    }
    if info.ahead > 0 {
        full_body.push_str(&format!(" {}{}", t.ic_up, info.ahead));
    }
    if info.behind > 0 {
        full_body.push_str(&format!(" {}{}", t.ic_down, info.behind));
    }

    let compact_body = if info.has_status && info.dirty {
        format!("{}*", info.branch)
    } else {
        info.branch.clone()
    };
    let min_body = info.branch;

    Item::new(
        4,
        format!("{color}{} {full_body}{}", t.ic_git, t.c_reset),
        format!("{color}{} {compact_body}{}", t.ic_git, t.c_reset),
        format!("{color}{} {min_body}{}", t.ic_git, t.c_reset),
    )
}

fn build_context_item(data: &JsonValue, t: &Theme) -> Option<Item> {
    let cw = data.get("context_window")?;
    let mut used_pct = cw
        .get("used_percentage")
        .and_then(|v| v.as_f64())
        .unwrap_or(-1.0);
    let win_size = cw
        .get("context_window_size")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let total_tokens = cw.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let in_tokens = cw
        .get("total_input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let out_tokens = cw
        .get("total_output_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let used_tokens = if used_pct > 0.0 && win_size > 0 {
        ((used_pct * win_size as f64) / 100.0).round() as i64
    } else if total_tokens > 0 {
        total_tokens
    } else if in_tokens > 0 {
        in_tokens
    } else {
        in_tokens + out_tokens
    };

    if used_pct < 0.0 && win_size > 0 && used_tokens > 0 {
        used_pct = (used_tokens as f64 / win_size as f64) * 100.0;
    }
    if used_pct < 0.0 {
        if used_tokens <= 0 {
            return None;
        }
        used_pct = 0.0;
    }

    let color = if used_pct >= 80.0 {
        t.c_crit
    } else if used_pct >= 50.0 {
        t.c_warn
    } else {
        t.c_context
    };

    let pct_str = format_percent(used_pct);
    let (full, compact) = if used_tokens > 0 {
        let tok_str = format_tokens(used_tokens);
        (
            format!(
                "{color}{} {pct_str}%{} {} {color}{tok_str} tokens{}",
                t.ic_context, t.c_sep, t.mid_dot, t.c_reset
            ),
            format!(
                "{color}{} {pct_str}%{} {} {color}{tok_str}{}",
                t.ic_context, t.c_sep, t.mid_dot, t.c_reset
            ),
        )
    } else {
        let s = format!("{color}{} {pct_str}%{}", t.ic_context, t.c_reset);
        (s.clone(), s)
    };
    let min = format!("{color}{} {pct_str}%{}", t.ic_context, t.c_reset);

    Item::new(5, full, compact, min)
}

fn build_cache_item(data: &JsonValue, t: &Theme) -> Option<Item> {
    let cur = data.get("context_window")?.get("current_usage")?;
    let cache_read = cur
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cache_create = cur
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let inp = cur.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let denom = inp + cache_create + cache_read;
    if denom <= 0 || cache_read <= 0 {
        return None;
    }
    let hit_pct = format_percent((cache_read as f64 / denom as f64) * 100.0);
    let full = format!("{}{} {hit_pct}% hit{}", t.c_cache, t.ic_cache, t.c_reset);
    let compact = format!("{}{} {hit_pct}%{}", t.c_cache, t.ic_cache, t.c_reset);
    Item::new(2, full, compact, String::new())
}

fn build_mode_item(data: &JsonValue, t: &Theme) -> Option<Item> {
    let mode = sanitize_json_str(data.get("cycle_mode"));
    if mode.is_empty() || mode == "default" {
        return None;
    }
    let low = mode.to_ascii_lowercase();
    let color = match low.as_str() {
        "plan" => t.c_model,
        "auto" => t.c_ok,
        "accept-edits" => t.c_warn,
        _ => t.c_muted,
    };
    let short = if low == "accept-edits" {
        "edits"
    } else {
        mode.as_str()
    };
    let full = format!("{color}{mode}{}", t.c_reset);
    let compact = format!("{color}{short}{}", t.c_reset);
    Item::new(3, full, compact.clone(), compact)
}

fn build_activity_item(data: &JsonValue, cfg: &Config, t: &Theme) -> Option<Item> {
    let mut parts = Vec::new();
    if cfg.show_subagents {
        if let Some(subs) = data.get("subagents").and_then(|v| v.as_array()) {
            let mut running = 0usize;
            for s in subs {
                let st = sanitize_json_str(s.get("status"));
                if st.is_empty() || st == "running" {
                    running += 1;
                }
            }
            if running > 0 {
                parts.push(format!(
                    "{}{} {running}{}",
                    t.c_activity, t.ic_subagents, t.c_reset
                ));
            }
        }
    }
    if cfg.show_tasks {
        let tasks = data.get("task_count").and_then(|v| v.as_i64()).unwrap_or(0);
        if tasks > 0 {
            parts.push(format!(
                "{}{} {tasks}{}",
                t.c_activity, t.ic_tasks, t.c_reset
            ));
        }
    }
    if cfg.show_artifacts {
        let arts = data
            .get("artifact_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if arts > 0 {
            parts.push(format!(
                "{}{} {arts}{}",
                t.c_muted, t.ic_artifacts, t.c_reset
            ));
        }
    }
    if parts.is_empty() {
        return None;
    }
    let sep = format!("{} {} {}", t.c_sep, t.mid_dot, t.c_reset);
    let joined = parts.join(&sep);
    Item::new(2, joined.clone(), joined, String::new())
}

struct QuotaEntry {
    label: String,
    pct: f64,
    used_pct: f64,
    reset_sec: i64,
}

fn get_quota_bucket_entry(
    quota_obj: &JsonValue,
    bucket_key: &str,
    label: &str,
    quota_mode: QuotaMode,
) -> Option<QuotaEntry> {
    let b = quota_obj.get(bucket_key)?;
    if b.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false) {
        return None;
    }
    let rf_raw = b.get("remaining_fraction")?.as_f64()?;
    if rf_raw < 0.0 {
        return None;
    }
    let rf = rf_raw.min(1.0);
    let used_pct = (1.0 - rf) * 100.0;
    let rem_pct = rf * 100.0;
    let display_pct = match quota_mode {
        QuotaMode::Used => used_pct,
        QuotaMode::Remaining => rem_pct,
    };

    let mut reset_sec = b
        .get("reset_in_seconds")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if reset_sec <= 0 {
        let rt_str = sanitize_json_str(b.get("reset_time"));
        if !rt_str.is_empty() {
            if let Some(target_sec) = parse_iso8601_unix_sec(&rt_str) {
                let diff = target_sec - now_unix_sec();
                if diff > 0 {
                    reset_sec = diff;
                }
            }
        }
    }

    Some(QuotaEntry {
        label: label.to_string(),
        pct: display_pct,
        used_pct,
        reset_sec,
    })
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| tok == needle)
}

fn build_quota_item(data: &JsonValue, cfg: &Config, t: &Theme) -> Option<Item> {
    let q_obj = data.get("quota")?;
    let model_obj = data.get("model");
    let model_hint = format!(
        "{} {}",
        sanitize_json_str(model_obj.and_then(|m| m.get("id"))),
        sanitize_json_str(model_obj.and_then(|m| m.get("display_name")))
    )
    .to_ascii_lowercase();

    const THIRD_PARTY_HINTS: &[&str] = &[
        "claude",
        "sonnet",
        "opus",
        "haiku",
        "gpt",
        "o1",
        "o3",
        "o4",
        "openai",
        "deepseek",
        "qwen",
        "kimi",
        "llama",
        "mistral",
        "grok",
        "anthropic",
    ];
    let is_third_party = THIRD_PARTY_HINTS
        .iter()
        .any(|kw| contains_word(&model_hint, kw))
        && !model_hint.contains("gemini");

    let preferred: [(&str, &str); 2] = if is_third_party {
        [("3p-5h", "5H"), ("3p-weekly", "7D")]
    } else {
        [("gemini-5h", "5H"), ("gemini-weekly", "7D")]
    };
    let fallback: [(&str, &str); 2] = if is_third_party {
        [("gemini-5h", "5H"), ("gemini-weekly", "7D")]
    } else {
        [("3p-5h", "5H"), ("3p-weekly", "7D")]
    };

    let mut entries = Vec::new();
    for (k, lbl) in preferred {
        if let Some(e) = get_quota_bucket_entry(q_obj, k, lbl, cfg.quota_mode) {
            entries.push(e);
        }
    }
    if entries.is_empty() {
        for (k, lbl) in fallback {
            if let Some(e) = get_quota_bucket_entry(q_obj, k, lbl, cfg.quota_mode) {
                entries.push(e);
            }
        }
    }
    if entries.is_empty() {
        if let Some(props) = q_obj.as_object_entries() {
            for (k, _) in props {
                let mut lbl = k
                    .strip_prefix("gemini-")
                    .or_else(|| k.strip_prefix("3p-"))
                    .unwrap_or(k)
                    .to_ascii_uppercase();
                if lbl == "WEEKLY" {
                    lbl = "7D".to_string();
                }
                if let Some(e) = get_quota_bucket_entry(q_obj, k, &lbl, cfg.quota_mode) {
                    entries.push(e);
                    if entries.len() >= 2 {
                        break;
                    }
                }
            }
        }
    }
    if entries.is_empty() {
        return None;
    }

    let bucket_color = |used_pct: f64| -> &'static str {
        if used_pct >= 90.0 {
            t.c_crit
        } else if used_pct >= 75.0 {
            t.c_warn
        } else {
            t.c_quota
        }
    };

    let max_used = entries
        .iter()
        .map(|e| e.used_pct)
        .fold(0.0f64, f64::max);
    let icon_color = bucket_color(max_used);

    if entries.len() == 1 {
        let e = &entries[0];
        let c = bucket_color(e.used_pct);
        let p = format_percent(e.pct);
        let cd = format_countdown(e.reset_sec);
        let full = if !cd.is_empty() {
            format!(
                "{icon_color}{} {c}{} {p}%{} ({cd}){}",
                t.ic_quota, e.label, t.c_muted, t.c_reset
            )
        } else {
            format!("{icon_color}{} {c}{} {p}%{}", t.ic_quota, e.label, t.c_reset)
        };
        let compact = format!("{icon_color}{} {c}{} {p}%{}", t.ic_quota, e.label, t.c_reset);
        let min = format!("{icon_color}{} {c}{p}%{}", t.ic_quota, t.c_reset);
        Item::new(3, full, compact, min)
    } else {
        let e0 = &entries[0];
        let e1 = &entries[1];
        let c0 = bucket_color(e0.used_pct);
        let c1 = bucket_color(e1.used_pct);
        let p0 = format_percent(e0.pct);
        let p1 = format_percent(e1.pct);
        let cd0 = format_countdown(e0.reset_sec);
        let cd1 = format_countdown(e1.reset_sec);
        let cd0_part = if !cd0.is_empty() {
            format!("{} ({cd0})", t.c_muted)
        } else {
            String::new()
        };
        let cd1_part = if !cd1.is_empty() {
            format!("{} ({cd1})", t.c_muted)
        } else {
            String::new()
        };
        let full = format!(
            "{icon_color}{} {c0}{} {p0}%{cd0_part}{} {} {c1}{} {p1}%{cd1_part}{}",
            t.ic_quota, e0.label, t.c_sep, t.mid_dot, e1.label, t.c_reset
        );
        let compact = format!(
            "{icon_color}{} {c0}{} {p0}%{cd0_part}{} {} {c1}{} {p1}%{}",
            t.ic_quota, e0.label, t.c_sep, t.mid_dot, e1.label, t.c_reset
        );
        let min = format!(
            "{icon_color}{} {c0}{} {p0}%{} {} {c1}{} {p1}%{}",
            t.ic_quota, e0.label, t.c_sep, t.mid_dot, e1.label, t.c_reset
        );
        Item::new(3, full, compact, min)
    }
}

fn build_cost_item(data: &JsonValue, t: &Theme) -> Option<Item> {
    let cost_obj = data.get("cost")?;
    let total_usd = cost_obj
        .get("total_usd")
        .or_else(|| cost_obj.get("total_cost_usd"))
        .and_then(|v| v.as_f64())
        .unwrap_or(-1.0);
    if total_usd <= 0.0 {
        return None;
    }

    let is_est = cost_obj
        .get("estimated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let prefix = if is_est { "~" } else { "" };
    let usd_str = format!("{prefix}{}", format_usd(total_usd));
    let icon_prefix = if !t.ic_cost.is_empty() {
        format!("{} ", t.ic_cost)
    } else {
        String::new()
    };

    let sub_usd = cost_obj
        .get("subagent_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let full = if sub_usd > 0.0 {
        format!(
            "{}{icon_prefix}{usd_str}{} (sub {}){}",
            t.c_cost,
            t.c_muted,
            format_usd(sub_usd),
            t.c_reset
        )
    } else {
        format!("{}{icon_prefix}{usd_str}{}", t.c_cost, t.c_reset)
    };
    let compact = format!("{}{icon_prefix}{usd_str}{}", t.c_cost, t.c_reset);
    Item::new(1, full, compact, String::new())
}

// --- Responsive Layout Engine ------------------------------------------------

fn join_items(items: &[Item], levels: &[usize], sep: &str) -> String {
    let mut parts = Vec::with_capacity(items.len());
    for (item, &lvl) in items.iter().zip(levels.iter()) {
        let txt = &item.variants[lvl];
        if !txt.is_empty() {
            parts.push(txt.as_str());
        }
    }
    parts.join(sep)
}

fn row_width(left_str: &str, right_str: &str) -> usize {
    let lw = display_width(left_str);
    let rw = display_width(right_str);
    if lw > 0 && rw > 0 {
        lw + 2 + rw
    } else {
        lw + rw
    }
}

fn format_aligned_row(left_str: &str, right_str: &str, budget: usize) -> String {
    let lw = display_width(left_str);
    let rw = display_width(right_str);
    if rw == 0 {
        return left_str.to_string();
    }
    if lw == 0 {
        return right_str.to_string();
    }
    let gap = budget.saturating_sub(lw + rw).max(2);
    format!("{left_str}{}{right_str}", " ".repeat(gap))
}

fn shrink_side(
    items: &[Item],
    levels: &mut [usize],
    target_width: usize,
    other_str: &str,
    sep: &str,
) {
    if items.is_empty() {
        return;
    }
    let mut order = Vec::with_capacity(items.len());
    for p in 1..=5u8 {
        for i in (0..items.len()).rev() {
            if items[i].priority == p {
                order.push(i);
            }
        }
    }
    for pass in [1usize, 2usize] {
        for &idx in &order {
            let cur = join_items(items, levels, sep);
            if row_width(&cur, other_str) <= target_width {
                return;
            }
            if levels[idx] < pass {
                levels[idx] = pass;
            }
        }
    }
}

fn render_status_line(data: &JsonValue, cfg: &Config, t: &Theme) -> String {
    let mut left_items = Vec::new();
    let mut right_items = Vec::new();

    if cfg.show_model {
        if let Some(it) = build_model_item(data, cfg, t) {
            left_items.push(it);
        }
    }
    if cfg.show_dir {
        if let Some(it) = build_dir_item(data, cfg, t) {
            left_items.push(it);
        }
    }
    if cfg.show_git {
        if let Some(it) = build_git_item(data, cfg, t) {
            left_items.push(it);
        }
    }
    if cfg.show_context {
        if let Some(it) = build_context_item(data, t) {
            left_items.push(it);
        }
    }
    if cfg.show_cache {
        if let Some(it) = build_cache_item(data, t) {
            left_items.push(it);
        }
    }

    if cfg.show_mode {
        if let Some(it) = build_mode_item(data, t) {
            right_items.push(it);
        }
    }
    if cfg.show_subagents || cfg.show_tasks || cfg.show_artifacts {
        if let Some(it) = build_activity_item(data, cfg, t) {
            right_items.push(it);
        }
    }
    if cfg.show_quota {
        if let Some(it) = build_quota_item(data, cfg, t) {
            right_items.push(it);
        }
    }
    if cfg.show_cost {
        if let Some(it) = build_cost_item(data, t) {
            right_items.push(it);
        }
    }

    if left_items.is_empty() && right_items.is_empty() {
        return format!("{}{} Antigravity{}", t.c_model, t.ic_model, t.c_reset);
    }

    let mut raw_width = cfg.width_override;
    if raw_width == 0 {
        let tw = data
            .get("terminal_width")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if tw > 0 {
            raw_width = tw as usize;
        }
    }
    if raw_width == 0 {
        if let Ok(cols) = env::var("COLUMNS") {
            if let Ok(c) = cols.trim().parse::<usize>() {
                if c > 0 {
                    raw_width = c;
                }
            }
        }
    }
    if raw_width == 0 {
        raw_width = 100;
    }
    let budget = raw_width.saturating_sub(2).max(40);

    let mut left_levels = vec![0usize; left_items.len()];
    let mut right_levels = vec![0usize; right_items.len()];
    let mut left_str = join_items(&left_items, &left_levels, &t.sep_str);
    let mut right_str = join_items(&right_items, &right_levels, &t.sep_str);

    // Step 1: Fits on one line as-is.
    if row_width(&left_str, &right_str) <= budget {
        return format_aligned_row(&left_str, &right_str, budget);
    }

    // Step 2: Shrink right side first.
    if !right_items.is_empty() {
        shrink_side(&right_items, &mut right_levels, budget, &left_str, &t.sep_str);
        right_str = join_items(&right_items, &right_levels, &t.sep_str);
        if row_width(&left_str, &right_str) <= budget {
            return format_aligned_row(&left_str, &right_str, budget);
        }
    }

    // Step 3: Shrink left side (lowest priority first).
    if !left_items.is_empty() {
        shrink_side(&left_items, &mut left_levels, budget, &right_str, &t.sep_str);
        left_str = join_items(&left_items, &left_levels, &t.sep_str);
        if row_width(&left_str, &right_str) <= budget {
            return format_aligned_row(&left_str, &right_str, budget);
        }
    }

    // Step 4: Wrap right group to line 2 and restore left/right to fullest form that fits.
    if !right_items.is_empty() && !left_items.is_empty() {
        let mut l_lvls = vec![0usize; left_items.len()];
        left_str = join_items(&left_items, &l_lvls, &t.sep_str);
        if display_width(&left_str) > budget {
            shrink_side(&left_items, &mut l_lvls, budget, "", &t.sep_str);
            left_str = join_items(&left_items, &l_lvls, &t.sep_str);
        }
        let mut r_lvls = vec![0usize; right_items.len()];
        right_str = join_items(&right_items, &r_lvls, &t.sep_str);
        if display_width(&right_str) > budget {
            shrink_side(&right_items, &mut r_lvls, budget, "", &t.sep_str);
            right_str = join_items(&right_items, &r_lvls, &t.sep_str);
        }
        if right_str.is_empty() {
            return left_str;
        }
        return format!("{left_str}\n{right_str}");
    }

    left_str
}

// --- Diagnostic Commands & Main Entrypoint -----------------------------------

fn write_stdout_line(text: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn print_legend(t: &Theme) {
    let cost_sample = if !t.ic_cost.is_empty() {
        format!("{} ~$0.42", t.ic_cost)
    } else {
        "~$0.42".to_string()
    };
    let lines = [
        format!("agyline v{SCRIPT_VERSION} (Rust)"),
        String::new(),
        "Left segments (cometix style):".to_string(),
        format!(
            "  {}{} Model {} Effort{}          Active model & reasoning effort (ANSI 14)",
            t.c_model, t.ic_model, t.mid_dot, t.c_reset
        ),
        format!(
            "  {}{} {}dir{}                     Current workspace directory (ANSI 11 icon + ANSI 10 text)",
            t.c_dir_icon, t.ic_dir, t.c_dir, t.c_reset
        ),
        format!(
            "  {}{} branch {} {}2{}             Git branch, status ({}/{}/{}), ahead/behind (ANSI 12, 5s SWR cache)",
            t.c_git, t.ic_git, t.ic_dirty, t.ic_up, t.c_reset, t.ic_clean, t.ic_dirty, t.ic_conflict
        ),
        format!(
            "  {}{} 62% {} 124k tokens{}     Context usage (ANSI 13 -> yellow >=50% -> red >=80%)",
            t.c_context, t.ic_context, t.mid_dot, t.c_reset
        ),
        format!(
            "  {}{} 81% hit{}                 Current-turn prompt cache hit ratio (ANSI 4)",
            t.c_cache, t.ic_cache, t.c_reset
        ),
        String::new(),
        "Right segments (Antigravity native, hidden when empty):".to_string(),
        format!(
            "  {}plan{} / {}auto{} / {}edits{}          Active cycle_mode (Shift+Tab)",
            t.c_model, t.c_reset, t.c_ok, t.c_reset, t.c_warn, t.c_reset
        ),
        format!(
            "  {}{} 2{} {}{}{} {}{} 1{} {}{}{} {}{} 3{}         Running subagents, background tasks, artifacts",
            t.c_activity, t.ic_subagents, t.c_reset,
            t.c_sep, t.mid_dot, t.c_reset,
            t.c_activity, t.ic_tasks, t.c_reset,
            t.c_sep, t.mid_dot, t.c_reset,
            t.c_muted, t.ic_artifacts, t.c_reset
        ),
        format!(
            "  {}{} 5H 40%{} (2h25m){} {} {}7D 22%{} (6d16h){}   Model-aware 5H/7D quota used % & reset countdown (ANSI 6)",
            t.c_quota, t.ic_quota, t.c_muted, t.c_sep, t.mid_dot, t.c_quota, t.c_muted, t.c_reset
        ),
        format!(
            "  {}{cost_sample}{}                  Estimated session cost in USD (ANSI 3)",
            t.c_cost, t.c_reset
        ),
        String::new(),
        "Flags:".to_string(),
        "  --classic, --no-nerdfont       Fallback to Unicode symbols".to_string(),
        "  --dir-style name|parent|tilde  Directory formatting style (default: name)".to_string(),
        "  --quota-mode used|remaining    Show quota as used % (default) or remaining %".to_string(),
        "  --fast-git                     Read .git/HEAD only (skip git status subprocess)".to_string(),
        "  --no-model, --no-dir, --no-git, --no-context, --no-cache".to_string(),
        "  --no-mode, --no-vim, --no-activity (--no-subagents/--no-tasks/--no-artifacts)".to_string(),
        "  --no-quota, --no-cost, --no-title, --no-watch".to_string(),
        "  --init-config, --preview [--width N], --legend, --version".to_string(),
    ];
    write_stdout_line(&lines.join("\n"));
}

fn print_preview(cfg: &Config, t: &Theme) {
    let samples = [
        (
            "Wide (140 cols) - Active session with quota & cost",
            r#"{"cwd":"C:\\Users\\dev\\projects\\demo","model":{"id":"Gemini 4 Neon (High)","display_name":"Gemini 4 Neon (High)","effort":"high"},"workspace":{"current_dir":"C:\\Users\\dev\\projects\\demo"},"context_window":{"total_input_tokens":88244,"total_output_tokens":61074,"context_window_size":1048576,"used_percentage":14.24,"current_usage":{"input_tokens":63382,"output_tokens":346,"cache_creation_input_tokens":0,"cache_read_input_tokens":20857}},"quota":{"gemini-5h":{"remaining_fraction":0.60,"reset_in_seconds":8700},"gemini-weekly":{"remaining_fraction":0.78,"reset_in_seconds":576000}},"vcs":{"type":"git","branch":"main","dirty":true},"cycle_mode":"plan","subagents":[{"name":"research","status":"running"},{"name":"test","status":"completed"}],"task_count":1,"artifact_count":2,"cost":{"total_usd":0.4213,"subagent_usd":0,"estimated":true},"terminal_width":150}"#,
        ),
        (
            "Medium (105 cols) - Third-party model with 5h + weekly quota & high context",
            r#"{"cwd":"C:\\Users\\dev\\Work\\app","model":{"id":"claude-opus-4-6","display_name":"Opus 4.6","effort":"xhigh"},"workspace":{"current_dir":"C:\\Users\\dev\\Work\\app"},"context_window":{"total_input_tokens":124000,"context_window_size":200000,"used_percentage":62,"current_usage":{"input_tokens":4000,"output_tokens":500,"cache_creation_input_tokens":1000,"cache_read_input_tokens":95000}},"quota":{"3p-5h":{"remaining_fraction":0.22,"reset_in_seconds":11520},"3p-weekly":{"remaining_fraction":0.78,"reset_in_seconds":420000}},"vcs":{"type":"git","branch":"feat/statusline","dirty":false},"vim":{"mode":"NORMAL"},"cost":{"total_usd":1.28,"subagent_usd":0.14,"estimated":false},"terminal_width":105}"#,
        ),
        (
            "Narrow (78 cols) - Graceful shrink & wrap",
            r#"{"cwd":"C:\\Users\\dev\\projects\\demo","model":{"id":"Gemini 4 Argon (Medium)","display_name":"Gemini 4 Argon (Medium)","effort":"medium"},"workspace":{"current_dir":"C:\\Users\\dev\\projects\\demo"},"context_window":{"total_input_tokens":180000,"context_window_size":200000,"used_percentage":86,"current_usage":{"input_tokens":20000,"output_tokens":500,"cache_creation_input_tokens":0,"cache_read_input_tokens":160000}},"quota":{"gemini-5h":{"remaining_fraction":0.08,"reset_in_seconds":1800}},"vcs":{"type":"git","branch":"main","dirty":true},"cycle_mode":"auto","cost":{"total_usd":2.5,"estimated":false},"terminal_width":78}"#,
        ),
    ];

    for (title, raw_json) in samples {
        let data = JsonParser::parse(raw_json).unwrap_or(JsonValue::Object(Vec::new()));
        write_stdout_line(&format!("{}--- {title} ---{}", t.c_muted, t.c_reset));
        write_stdout_line(&render_status_line(&data, cfg, t));
        write_stdout_line("");
    }
}

fn truncate_title(s: &str, max_cols: usize) -> String {
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    let limit = max_cols.saturating_sub(1);
    let mut out = String::new();
    let mut cols = 0usize;
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let cw = display_width(ch.encode_utf8(&mut buf));
        if cols + cw > limit {
            break;
        }
        out.push(ch);
        cols += cw;
    }
    out.trim_end().to_string() + "\u{2026}"
}

fn resolve_terminal_title(data: &JsonValue) -> String {
    let conv_title = sanitize_json_str(data.get("conversation_title"));
    if !conv_title.is_empty() {
        return truncate_title(&conv_title, 60);
    }
    let dir = extract_dir(data);
    if !dir.is_empty() {
        let leaf = dir
            .split(|c| c == '\\' || c == '/')
            .filter(|p| !p.is_empty())
            .last()
            .unwrap_or(dir.as_str());
        if !leaf.is_empty() {
            return truncate_title(leaf, 60);
        }
    }
    "Antigravity".to_string()
}

fn set_terminal_title(title: &str) {
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        if title.is_empty() {
            return;
        }
        let wide: Vec<u16> = OsStr::new(title).encode_wide().chain(Some(0)).collect();
        unsafe {
            SetConsoleTitleW(wide.as_ptr());
        }
    }
    #[cfg(not(windows))]
    {
        let _ = title;
    }
}

fn log_unexpected_error(msg: &str) {
    let err_log = env::temp_dir().join("agy-statusline-err.log");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(err_log) {
        let _ = writeln!(f, "{} {msg}", now_unix_ms());
    }
}

fn init_sidecar_config() {
    let Some(path) = target_init_config_path() else {
        write_stdout_line("Unable to resolve executable directory.");
        return;
    };
    if path.exists() {
        write_stdout_line(&format!("Config already exists: {}", path.display()));
        return;
    }
    match fs::write(&path, DEFAULT_CONFIG_TOML) {
        Ok(()) => write_stdout_line(&format!("Created config: {}", path.display())),
        Err(e) => write_stdout_line(&format!("Failed to write {}: {e}", path.display())),
    }
}

fn run() {
    #[cfg(windows)]
    unsafe {
        SetConsoleOutputCP(65001);
    }

    let cfg = Config::load_with_args(env::args().skip(1));
    let theme = Theme::new(cfg.use_classic, cfg.no_color);

    match cfg.action_mode {
        ActionMode::Version => {
            write_stdout_line(&format!("agyline v{SCRIPT_VERSION} (Rust)"));
        }
        ActionMode::Legend => {
            print_legend(&theme);
        }
        ActionMode::Preview => {
            print_preview(&cfg, &theme);
        }
        ActionMode::InitConfig => {
            init_sidecar_config();
        }
        ActionMode::RefreshGit => {
            refresh_git_worker(&cfg.refresh_git_dir, &cfg);
        }
        ActionMode::ReapOrphans => {
            reap_orphans_worker(cfg.reap_min_age.unwrap_or(cfg.reap_min_age_secs));
        }
        ActionMode::WatchAgy => {
            #[cfg(windows)]
            win_watch::run_watcher(win_watch::WatcherOptions {
                poll_ms: cfg.watch_poll_ms,
                exit_settle_ms: cfg.exit_settle_ms,
                reap_interval_secs: cfg.reap_interval_secs,
                reap_min_age_secs: cfg.reap_min_age_secs,
            });
        }
        ActionMode::Render => {
            let input_json = read_bounded_stdin(STDIN_TIMEOUT_MS);
            let payload =
                JsonParser::parse(&input_json).unwrap_or_else(|_| JsonValue::Object(Vec::new()));
            if cfg.set_title {
                let title = resolve_terminal_title(&payload);
                set_terminal_title(&title);
            }
            let rendered = render_status_line(&payload, &cfg, &theme);
            write_stdout_line(&rendered);
            maybe_spawn_detached_reap(&cfg);
        }
    }
}

fn main() {
    if let Err(payload) = std::panic::catch_unwind(run) {
        let msg = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        log_unexpected_error(&msg);
        write_stdout_line("Antigravity");
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_standard_vector() {
        assert_eq!(sha256_hex_16("abc"), "ba7816bf8f01cfea");
        assert_eq!(sha256_hex_16(""), "e3b0c44298fc1c14");
    }

    #[test]
    fn test_json_parser_and_surrogates() {
        let raw = r#"{"a":1.5,"b":true,"c":"hello \u0041 \uD83D\uDE00","arr":[1,2,3],"nested":{"x":"y"}}"#;
        let v = JsonParser::parse(raw).expect("valid json");
        assert_eq!(v.get("a").and_then(|x| x.as_f64()), Some(1.5));
        assert_eq!(v.get("b").and_then(|x| x.as_bool()), Some(true));
        assert_eq!(v.get("c").and_then(|x| x.as_str()), Some("hello A \u{1F600}"));
        assert_eq!(v.get("arr").and_then(|x| x.as_array()).map(|a| a.len()), Some(3));
        assert_eq!(
            v.get("nested").and_then(|n| n.get("x")).and_then(|x| x.as_str()),
            Some("y")
        );
    }

    #[test]
    fn test_ansi_sanitization_and_width() {
        let dirty = "\x1b[31mhello\x1b[0m\x1b]0;title\x07\x01world";
        assert_eq!(sanitize_string(dirty), "helloworld");
        assert_eq!(display_width("\x1b[96m\u{F0629} Gemini\x1b[0m"), 8);
        assert_eq!(display_width("\x1b[96m\u{E26D} Gemini\x1b[0m"), 8);
        assert_eq!(display_width("中文ab"), 6);
    }

    #[test]
    fn test_quota_used_and_remaining() {
        let raw = r#"{"model":{"id":"gemini-4"},"quota":{"gemini-5h":{"remaining_fraction":0.60,"reset_in_seconds":8700},"gemini-weekly":{"remaining_fraction":0.78,"reset_in_seconds":576000}}}"#;
        let v = JsonParser::parse(raw).unwrap();
        let mut cfg = Config::default();
        cfg.no_color = true;
        cfg.use_classic = true;
        let t = Theme::new(true, true);

        let used = build_quota_item(&v, &cfg, &t).unwrap();
        assert!(used.variants[0].contains("5H 40% (2h25m)"));
        assert!(used.variants[0].contains("7D 22% (6d16h)"));

        cfg.quota_mode = QuotaMode::Remaining;
        let rem = build_quota_item(&v, &cfg, &t).unwrap();
        assert!(rem.variants[0].contains("5H 60% (2h25m)"));
        assert!(rem.variants[0].contains("7D 78% (6d16h)"));
    }

    #[test]
    fn test_usd_and_countdown() {
        assert_eq!(format_usd(0.00005), "<$0.0001");
        assert_eq!(format_usd(0.0042), "$0.0042");
        assert_eq!(format_usd(0.4213), "$0.42");
        assert_eq!(format_usd(2.5), "$2.50");
        assert_eq!(format_countdown(45), "<1m");
        assert_eq!(format_countdown(8700), "2h25m");
        assert_eq!(format_countdown(576000), "6d16h");
    }

    #[test]
    fn test_iso8601_parser() {
        let ts = parse_iso8601_unix_sec("2026-11-25T10:00:00Z").unwrap();
        let ts_tz = parse_iso8601_unix_sec("2026-11-25T18:00:00+08:00").unwrap();
        assert_eq!(ts, ts_tz);
    }

    #[test]
    fn test_model_dedup_and_responsive_fit() {
        let raw = r#"{"cwd":"C:\\Users\\dev\\Work\\app","model":{"id":"claude-opus-4-6","display_name":"Opus 4.6 (xHigh)","effort":"xhigh"},"workspace":{"current_dir":"C:\\Users\\dev\\Work\\app"},"context_window":{"total_input_tokens":124000,"context_window_size":200000,"used_percentage":62,"current_usage":{"input_tokens":4000,"output_tokens":500,"cache_creation_input_tokens":1000,"cache_read_input_tokens":95000}},"quota":{"3p-5h":{"remaining_fraction":0.22,"reset_in_seconds":11520},"3p-weekly":{"remaining_fraction":0.78,"reset_in_seconds":420000}},"vcs":{"type":"git","branch":"feat/statusline","dirty":false},"vim":{"mode":"NORMAL"},"cost":{"total_usd":1.28,"subagent_usd":0.14,"estimated":false},"terminal_width":105}"#;
        let v = JsonParser::parse(raw).unwrap();
        let mut cfg = Config::default();
        cfg.no_color = true;
        cfg.use_classic = true;
        let t = Theme::new(true, true);
        let out = render_status_line(&v, &cfg, &t);
        assert!(!out.contains('\n'), "expected 105-col layout to fit on one line: {out}");
        assert!(out.contains("Opus 4.6 \u{00B7} xHigh"));
        assert!(!out.contains("(xHigh)"));
        assert!(out.contains("5H 78%"));
        assert!(out.contains("7D 22%"));
    }

    #[test]
    fn test_model_family_icons() {
        let cfg_nerd = Config::default();
        let t_nerd = Theme::new(false, true);
        assert_eq!(resolve_model_icon("gemini 4 neon", &cfg_nerd, &t_nerd), "\u{F0AE2}");
        assert_eq!(resolve_model_icon("claude sonnet 5.5", &cfg_nerd, &t_nerd), "\u{EC82}");
        assert_eq!(resolve_model_icon("opus 4.6", &cfg_nerd, &t_nerd), "\u{EC82}");
        assert_eq!(resolve_model_icon("gpt-5 codex", &cfg_nerd, &t_nerd), "\u{EC81}");
        assert_eq!(resolve_model_icon("o3-mini", &cfg_nerd, &t_nerd), "\u{EC81}");
        assert_eq!(resolve_model_icon("deepseek-r1", &cfg_nerd, &t_nerd), "\u{F18B4}");
        assert_eq!(resolve_model_icon("github copilot", &cfg_nerd, &t_nerd), "\u{EC1E}");
        assert_eq!(resolve_model_icon("qwen-3-coder", &cfg_nerd, &t_nerd), "\u{F0629}");

        let mut cfg_classic = Config::default();
        cfg_classic.use_classic = true;
        let t_classic = Theme::new(true, true);
        assert_eq!(resolve_model_icon("gemini 4 neon", &cfg_classic, &t_classic), "\u{25C6}");
    }

    #[test]
    fn test_resolve_terminal_title() {
        let with_title = JsonParser::parse(
            r#"{"conversation_title":"Diagnose statusline & theme","workspace":{"current_dir":"C:\\Work\\demo"}}"#,
        )
        .unwrap();
        assert_eq!(resolve_terminal_title(&with_title), "Diagnose statusline & theme");

        let without_title =
            JsonParser::parse(r#"{"workspace":{"current_dir":"C:\\Work\\demo"}}"#).unwrap();
        assert_eq!(resolve_terminal_title(&without_title), "demo");

        let empty = JsonParser::parse(r#"{}"#).unwrap();
        assert_eq!(resolve_terminal_title(&empty), "Antigravity");

        let long_cjk = "这是一个非常非常非常非常非常非常非常非常非常非常长的中文会话标题用于测试截断";
        let truncated = truncate_title(long_cjk, 20);
        assert!(truncated.ends_with('\u{2026}'));
        assert!(display_width(&truncated) <= 20);
    }

    #[cfg(windows)]
    #[test]
    fn test_win_reap_struct_layout_and_path_norm() {
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<win_reap::ProcessEntry32W>(), 568);
        let base = win_reap::normalize_win_path(r"\\?\C:\Users\Dev\.antigravity\statusline.exe");
        assert_eq!(base, r"c:\users\dev\.antigravity\statusline.exe");
        assert_eq!(
            win_reap::normalize_win_path("C:/Users/Dev/.antigravity/STATUSLINE.EXE "),
            base
        );
        assert!(win_reap::is_self_or_old_backup(&base, &base));
        assert!(win_reap::is_self_or_old_backup(
            r"c:\users\dev\.antigravity\statusline.exe.old.130902677",
            &base
        ));
        assert!(!win_reap::is_self_or_old_backup(
            r"c:\users\dev\.antigravity\statusline.exe.old.",
            &base
        ));
        assert!(!win_reap::is_self_or_old_backup(
            r"c:\other\statusline.exe.old.130902677",
            &base
        ));
    }

    #[test]
    fn test_parse_watch_and_min_age_flags() {
        let cfg = Config::parse_args(vec![
            "--reap-orphans".to_string(),
            "--min-age".to_string(),
            "0".to_string(),
            "--no-watch".to_string(),
        ]);
        assert!(cfg.action_mode == ActionMode::ReapOrphans);
        assert_eq!(cfg.reap_min_age, Some(0));
        assert!(!cfg.watch_agy);

        let watch_cfg = Config::parse_args(vec!["--watch-agy".to_string()]);
        assert!(watch_cfg.action_mode == ActionMode::WatchAgy);
        assert!(watch_cfg.watch_agy);

        let init_cfg = Config::parse_args(vec!["--init-config".to_string()]);
        assert!(init_cfg.action_mode == ActionMode::InitConfig);
    }

    #[test]
    fn test_toml_config_parsing_clamping_and_cli_override() {
        let toml = r#"
            # comment line
            glyphs = "classic" # inline comment
            title = false

            [layout]
            width = 120
            dir_style = "parent"
            quota_mode = "remaining"

            [git]
            fast = true
            cache_ms = 2000
            timeout_ms = 100 # clamped to 500

            [watcher]
            enabled = true
            poll_ms = 500 # clamped to 1000
            exit_settle_ms = 800
            reap_interval_secs = 90
            reap_min_age_secs = 5

            [segments]
            cost = false
            vim = false
        "#;
        let mut cfg = Config::default();
        cfg.apply_toml(toml);
        assert!(cfg.use_classic);
        assert!(!cfg.set_title);
        assert_eq!(cfg.width_override, 120);
        assert!(cfg.dir_style == DirStyle::Parent);
        assert!(cfg.quota_mode == QuotaMode::Remaining);
        assert!(cfg.fast_git);
        assert_eq!(cfg.git_cache_ms, 2000);
        assert_eq!(cfg.git_timeout_ms, 500);
        assert!(cfg.watch_agy);
        assert_eq!(cfg.watch_poll_ms, 1000);
        assert_eq!(cfg.exit_settle_ms, 800);
        assert_eq!(cfg.reap_interval_secs, 90);
        assert_eq!(cfg.reap_min_age_secs, 5);
        assert!(!cfg.show_cost);
        assert!(!cfg.show_vim);
        assert!(cfg.show_model);

        // CLI flags must override TOML values.
        cfg.apply_args(vec!["--no-watch".to_string(), "--width".to_string(), "96".to_string()]);
        assert!(!cfg.watch_agy);
        assert_eq!(cfg.width_override, 96);

        // Built-in default template must parse cleanly without panics.
        let mut def_cfg = Config::default();
        def_cfg.apply_toml(DEFAULT_CONFIG_TOML);
        assert_eq!(def_cfg.watch_poll_ms, WATCH_POLL_DEFAULT_MS);
    }
}
