use super::{sanitize_json_str, terminal_title_name, truncate_title, JsonValue};

const FRAME_MS: u64 = 1_000;
const DONE_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Working,
    Starting,
    SigningIn,
    Background,
    Attention,
    Done,
    Idle,
    Error,
    Unknown,
}

impl Status {
    fn from_payload(data: &JsonValue) -> Self {
        if data
            .get("tool_confirmation_pending")
            .and_then(JsonValue::as_bool)
            == Some(true)
        {
            return Self::Attention;
        }
        match sanitize_json_str(data.get("agent_state"))
            .to_ascii_lowercase()
            .as_str()
        {
            "working" | "tool_use" | "reviewing" => Self::Working,
            "initializing" => Self::Starting,
            "authenticating" => Self::SigningIn,
            "error" => Self::Error,
            "idle" => {
                let background = data
                    .get("task_count")
                    .and_then(JsonValue::as_i64)
                    .unwrap_or(0)
                    > 0
                    || data
                        .get("subagents")
                        .and_then(JsonValue::as_array)
                        .is_some_and(|subs| {
                            subs.iter().any(|sub| {
                                let status = sanitize_json_str(sub.get("status"));
                                status.is_empty() || status == "running"
                            })
                        });
                if background {
                    Self::Background
                } else {
                    Self::Idle
                }
            }
            _ => Self::Unknown,
        }
    }

    #[cfg(any(windows, test))]
    fn animated(self) -> bool {
        matches!(
            self,
            Self::Working | Self::Starting | Self::SigningIn | Self::Background | Self::Attention
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TitleState {
    conversation: String,
    name: String,
    status: Status,
    since_ms: u64,
    bell_pending: bool,
}

impl TitleState {
    fn update(previous: Option<&Self>, data: &JsonValue, now_ms: u64) -> Self {
        let conversation = super::sha256_hex_16(
            data.get("conversation_id")
                .or_else(|| data.get("session_id"))
                .and_then(JsonValue::as_str)
                .unwrap_or(""),
        );
        let previous = previous.filter(|old| old.conversation == conversation);
        let mut status = Status::from_payload(data);
        if status == Status::Idle
            && previous.is_some_and(|old| {
                matches!(
                    old.status,
                    Status::Working | Status::Background | Status::Attention | Status::Done
                )
            })
        {
            // shortcut: agy exposes idle, not success or acknowledgement. Clear after five seconds.
            status = Status::Done;
        }
        let since_ms = previous
            .filter(|old| old.status == status)
            .map_or(now_ms, |old| old.since_ms);
        let bell_pending = previous
            .filter(|old| old.status == status)
            .map_or(matches!(status, Status::Done | Status::Attention), |old| {
                old.bell_pending
            });
        Self {
            conversation,
            name: terminal_title_name(data),
            status,
            since_ms,
            bell_pending,
        }
    }

    fn render(&self, now_ms: u64) -> String {
        let elapsed = now_ms.saturating_sub(self.since_ms);
        let second_frame = (elapsed / FRAME_MS) % 2 == 1;
        let working = if second_frame { "◑" } else { "◐" };
        let prefix = match self.status {
            Status::Working | Status::Starting | Status::SigningIn | Status::Background => working,
            Status::Attention => {
                if second_frame {
                    "[ . ]"
                } else {
                    "[ ! ]"
                }
            }
            Status::Done if elapsed < DONE_MS => "🟢",
            Status::Error => "🔴",
            _ => "",
        };
        if prefix.is_empty() {
            self.name.clone()
        } else {
            truncate_title(&format!("{prefix} {}", self.name), 60)
        }
    }

    #[cfg(any(windows, test))]
    fn next_tick_ms(&self, now_ms: u64) -> Option<u64> {
        let elapsed = now_ms.saturating_sub(self.since_ms);
        if self.status.animated() {
            Some(FRAME_MS - elapsed % FRAME_MS)
        } else if self.status == Status::Done && elapsed < DONE_MS {
            Some(DONE_MS - elapsed)
        } else {
            None
        }
    }
}

pub(super) fn resolve(data: &JsonValue) -> String {
    TitleState::update(None, data, 0).render(0)
}

pub(super) fn prepare(data: &JsonValue, watch: bool) -> String {
    #[cfg(windows)]
    if let Some(title) = windows::publish(data, watch) {
        return title;
    }
    #[cfg(not(windows))]
    let _ = watch;
    resolve(data)
}

#[cfg(windows)]
pub(super) mod windows {
    use super::*;
    use crate::{fs, json_escape, JsonParser};
    use std::ffi::c_void;
    use std::io::Read;
    use std::os::windows::io::AsRawHandle;
    use std::path::PathBuf;
    use std::ptr;
    use std::time::Duration;

    const EVENT_NAME: &str = "Local\\agyline-title-update-v1";
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const QUERY_LIMITED: u32 = 0x1000;
    const WAIT_TIMEOUT: u32 = 258;

    extern "system" {
        fn CreateEventW(
            attrs: *mut c_void,
            manual: i32,
            initial: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn SetEvent(handle: *mut c_void) -> i32;
        fn CreateMutexW(attrs: *mut c_void, owner: i32, name: *const u16) -> *mut c_void;
        fn ReleaseMutex(handle: *mut c_void) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, timeout: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn GetTickCount64() -> u64;
        fn AttachConsole(pid: u32) -> i32;
        fn FreeConsole() -> i32;
        fn SetConsoleCtrlHandler(handler: *mut c_void, add: i32) -> i32;
        fn GetConsoleTitleW(title: *mut u16, size: u32) -> u32;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn WriteConsoleW(
            output: *mut c_void,
            buffer: *const c_void,
            count: u32,
            written: *mut u32,
            reserved: *mut c_void,
        ) -> i32;
    }

    pub(crate) struct Handle(pub(crate) *mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    fn now_ms() -> u64 {
        unsafe { GetTickCount64() }
    }
    fn state_dir() -> PathBuf {
        std::env::temp_dir().join("agyline-titles-v1")
    }

    pub(crate) fn update_event() -> Option<Handle> {
        let handle = unsafe { CreateEventW(ptr::null_mut(), 0, 0, wide(EVENT_NAME).as_ptr()) };
        if handle.is_null() {
            None
        } else {
            Some(Handle(handle))
        }
    }

    struct Session {
        process: Handle,
        pid: u32,
        created: u64,
    }

    impl Session {
        fn open(pid: u32) -> Option<Self> {
            let handle = unsafe { OpenProcess(SYNCHRONIZE | QUERY_LIMITED, 0, pid) };
            if handle.is_null() {
                return None;
            }
            let process = Handle(handle);
            if unsafe { WaitForSingleObject(handle, 0) } != WAIT_TIMEOUT {
                return None;
            }
            let created = crate::win_reap::process_creation_time(handle)?;
            Some(Self {
                process,
                pid,
                created,
            })
        }

        fn key(&self) -> String {
            format!("{}-{}", self.pid, self.created)
        }
        fn path(&self) -> PathBuf {
            state_dir().join(format!("{}.json", self.key()))
        }

        fn lock(&self) -> Option<SessionLock> {
            let name = wide(&format!("Local\\agyline-title-{}", self.key()));
            let handle = unsafe { CreateMutexW(ptr::null_mut(), 0, name.as_ptr()) };
            if handle.is_null() {
                return None;
            }
            let handle = Handle(handle);
            // Abandoned mutexes are acquired too. A killed callback may leave an incomplete file.
            match unsafe { WaitForSingleObject(handle.0, 50) } {
                0 | 0x80 => Some(SessionLock(handle)),
                _ => None,
            }
        }

        fn read(&self) -> Option<TitleState> {
            let mut text = String::new();
            fs::File::open(self.path())
                .ok()?
                .take(16_384)
                .read_to_string(&mut text)
                .ok()?;
            decode(&text)
        }

        fn set_title(&self, title: &str, bell: bool) {
            // Keep the process handle open to prevent PID reuse while routing this write.
            if unsafe { WaitForSingleObject(self.process.0, 0) } != WAIT_TIMEOUT {
                return;
            }
            if unsafe { AttachConsole(self.pid) } == 0 {
                return;
            }
            // Attaching resets the control handler, so ignore Ctrl+C in each target console.
            unsafe {
                SetConsoleCtrlHandler(ptr::null_mut(), 1);
            }
            let mut current = [0u16; 1024];
            let len =
                unsafe { GetConsoleTitleW(current.as_mut_ptr(), current.len() as u32) } as usize;
            if String::from_utf16_lossy(&current[..len.min(current.len())]) != title {
                crate::set_terminal_title(title);
            }
            if bell {
                ring_console_bell();
            }
            detach_console();
        }
    }

    fn ring_console_bell() {
        // stdout belongs to agy's captured title command, so use the attached console directly.
        let Ok(output) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONOUT$")
        else {
            return;
        };
        let mut mode = 0;
        if unsafe { GetConsoleMode(output.as_raw_handle(), &mut mode) } == 0 || mode & 1 == 0 {
            return;
        }
        let bell = 7u16;
        let mut written = 0;
        unsafe {
            WriteConsoleW(
                output.as_raw_handle(),
                (&bell as *const u16).cast(),
                1,
                &mut written,
                ptr::null_mut(),
            );
        }
    }

    struct SessionLock(Handle);
    impl Drop for SessionLock {
        fn drop(&mut self) {
            unsafe {
                ReleaseMutex(self.0 .0);
            }
        }
    }

    fn encode(state: &TitleState) -> String {
        format!(
            r#"{{"conversation":{},"name":{},"status":"{:?}","since_ms":{},"bell_pending":{}}}"#,
            json_escape(&state.conversation),
            json_escape(&state.name),
            state.status,
            state.since_ms,
            state.bell_pending
        )
    }

    fn decode(text: &str) -> Option<TitleState> {
        let data = JsonParser::parse(text).ok()?;
        let conversation = data.get("conversation")?.as_str()?.to_string();
        if conversation.len() != 16 || !conversation.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let name = truncate_title(
            &crate::sanitize_terminal_title(data.get("name")?.as_str()?),
            60,
        );
        if name.is_empty() {
            return None;
        }
        let status = match data.get("status")?.as_str()? {
            "Working" => Status::Working,
            "Starting" => Status::Starting,
            "SigningIn" => Status::SigningIn,
            "Background" => Status::Background,
            "Attention" => Status::Attention,
            "Done" => Status::Done,
            "Idle" => Status::Idle,
            "Error" => Status::Error,
            "Unknown" => Status::Unknown,
            _ => return None,
        };
        let since_ms = u64::try_from(data.get("since_ms")?.as_i64()?).ok()?;
        Some(TitleState {
            conversation,
            name,
            status,
            since_ms,
            // Old cache entries must not produce retrospective notifications after an upgrade.
            bell_pending: data
                .get("bell_pending")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
        })
    }

    pub(crate) fn publish(data: &JsonValue, watch: bool) -> Option<String> {
        let (pid, created) = crate::win_reap::agy_ancestor()?;
        let session = Session::open(pid).filter(|s| s.created == created)?;
        let lock = session.lock()?;
        if !watch {
            let _ = fs::remove_file(session.path());
            return None;
        }
        fs::create_dir_all(state_dir()).ok()?;
        let now = now_ms();
        let state = TitleState::update(session.read().as_ref(), data, now);
        fs::write(session.path(), encode(&state)).ok()?;
        drop(lock);
        // Native agy writes this frame from stdout. The watcher supplies subsequent frames.
        crate::maybe_ensure_agy_watcher();
        if let Some(event) = update_event() {
            unsafe {
                SetEvent(event.0);
            }
        }
        Some(state.render(now))
    }

    pub(crate) fn detach_console() {
        unsafe {
            FreeConsole();
        }
    }

    /// Inspect only registered title files, never the global process table.
    pub(crate) fn tick() -> Option<Duration> {
        let entries = fs::read_dir(state_dir()).ok()?;
        let mut next_ms = None;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(key) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some((pid, created)) = key.split_once('-') else {
                continue;
            };
            let (Ok(pid), Ok(created)) = (pid.parse::<u32>(), created.parse::<u64>()) else {
                continue;
            };
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let session = Session::open(pid).filter(|s| s.created == created);
            let Some(session) = session else {
                let _ = fs::remove_file(path);
                continue;
            };
            let Some(_lock) = session.lock() else {
                next_ms = Some(next_ms.map_or(50, |ms: u64| ms.min(50)));
                continue;
            };
            let Some(mut state) = session.read() else {
                continue;
            };
            let now = now_ms();
            let mut bell = false;
            if state.bell_pending {
                state.bell_pending = false;
                // Consume before writing to the console, so a restart cannot repeat the bell.
                bell = fs::write(session.path(), encode(&state)).is_ok()
                    && now.saturating_sub(state.since_ms) < DONE_MS
                    && matches!(state.status, Status::Done | Status::Attention);
            }
            session.set_title(&state.render(now), bell);
            if let Some(delay) = state.next_tick_ms(now) {
                next_ms = Some(next_ms.map_or(delay, |ms| ms.min(delay)));
            }
        }
        next_ms.map(Duration::from_millis)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cache_round_trip_and_invalid_input() {
            let payload = JsonParser::parse(
                r#"{"agent_state":"working","conversation_title":"Quotes \" and 中文"}"#,
            )
            .unwrap();
            let state = TitleState::update(None, &payload, 1234);
            assert_eq!(decode(&encode(&state)), Some(state.clone()));
            assert!(decode(&encode(&state).replace("Working", "invalid")).is_none());
            assert!(decode(&encode(&state).replace("1234", "-1")).is_none());
            assert!(decode("{partial").is_none());
            let mut pending = state;
            pending.status = Status::Attention;
            pending.bell_pending = true;
            assert_eq!(decode(&encode(&pending)), Some(pending.clone()));
            let legacy = encode(&pending).replace(",\"bell_pending\":true", "");
            assert!(!decode(&legacy).unwrap().bell_pending);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JsonParser;

    fn payload(state: &str, conversation: &str) -> JsonValue {
        JsonParser::parse(&format!(r#"{{"agent_state":"{state}","conversation_id":"{conversation}","conversation_title":"Session {conversation}"}}"#)).unwrap()
    }

    #[test]
    fn fixed_frames_survive_callbacks_and_state_changes() {
        let working = TitleState::update(None, &payload("working", "A"), 100);
        assert_eq!(working.render(1099), "◐ Session A");
        assert_eq!(working.render(1100), "◑ Session A");
        let working = TitleState::update(Some(&working), &payload("tool_use", "A"), 1500);
        assert_eq!(working.render(2099), "◑ Session A");
        assert_eq!(working.render(2100), "◐ Session A");
        assert_eq!(working.next_tick_ms(1500), Some(600));
        let pending = JsonParser::parse(
            r#"{"conversation_id":"A","agent_state":"error","tool_confirmation_pending":true}"#,
        )
        .unwrap();
        let attention = TitleState::update(Some(&working), &pending, 1600);
        assert_eq!(attention.render(2599), "[ ! ] Antigravity");
        assert_eq!(attention.render(2600), "[ . ] Antigravity");
        assert_eq!(attention.render(3600), "[ ! ] Antigravity");
    }

    #[test]
    fn completion_expires_without_callback_and_does_not_restart() {
        let idle = payload("idle", "A");
        assert_eq!(TitleState::update(None, &idle, 0).render(0), "Session A");
        let working = TitleState::update(None, &payload("working", "A"), 100);
        let done = TitleState::update(Some(&working), &idle, 200);
        assert_eq!(done.render(5199), "🟢 Session A");
        assert_eq!(done.next_tick_ms(5199), Some(1));
        assert_eq!(done.render(5200), "Session A");
        assert_eq!(done.next_tick_ms(5200), None);
        let repeat = TitleState::update(Some(&done), &idle, 9000);
        assert_eq!(repeat.render(9000), "Session A");
        let restart = TitleState::update(Some(&done), &payload("working", "A"), 300);
        assert_eq!(restart.render(300), "◐ Session A");
    }

    #[test]
    fn conversation_error_and_startup_do_not_inherit_completion() {
        let working = TitleState::update(None, &payload("working", "A"), 0);
        let other = TitleState::update(Some(&working), &payload("idle", "B"), 100);
        assert_eq!(other.render(100), "Session B");
        let error = TitleState::update(Some(&working), &payload("error", "A"), 100);
        assert_eq!(error.render(100), "🔴 Session A");
        assert_eq!(error.next_tick_ms(100), None);
        let recovered = TitleState::update(Some(&error), &payload("idle", "A"), 200);
        assert_eq!(recovered.render(200), "Session A");
        let starting = TitleState::update(None, &payload("initializing", "A"), 0);
        assert_eq!(
            TitleState::update(Some(&starting), &payload("idle", "A"), 100).render(100),
            "Session A"
        );
    }

    #[test]
    fn bell_follows_transitions_and_survives_repeated_callbacks() {
        let working = TitleState::update(None, &payload("working", "A"), 0);
        assert!(!working.bell_pending);
        let idle = payload("idle", "A");
        let done = TitleState::update(Some(&working), &idle, 100);
        assert!(done.bell_pending);
        let mut repeated = TitleState::update(Some(&done), &idle, 200);
        assert!(repeated.bell_pending);
        repeated.bell_pending = false;
        assert!(!TitleState::update(Some(&repeated), &idle, 300).bell_pending);

        let pending =
            JsonParser::parse(r#"{"conversation_id":"A","tool_confirmation_pending":true}"#)
                .unwrap();
        let attention = TitleState::update(Some(&working), &pending, 400);
        assert!(attention.bell_pending);
        let mut repeated = TitleState::update(Some(&attention), &pending, 500);
        assert!(repeated.bell_pending);
        repeated.bell_pending = false;
        assert!(!TitleState::update(Some(&repeated), &pending, 600).bell_pending);
        let resumed = TitleState::update(Some(&attention), &payload("working", "A"), 700);
        assert!(!resumed.bell_pending);
        assert!(TitleState::update(Some(&resumed), &pending, 800).bell_pending);

        assert!(!TitleState::update(None, &idle, 0).bell_pending);
        assert!(!TitleState::update(Some(&done), &payload("idle", "B"), 900).bell_pending);
        assert!(!TitleState::update(Some(&working), &payload("error", "A"), 900).bell_pending);
        let background =
            JsonParser::parse(r#"{"conversation_id":"A","agent_state":"idle","task_count":1}"#)
                .unwrap();
        let background = TitleState::update(Some(&working), &background, 1000);
        assert!(!background.bell_pending);
        assert!(TitleState::update(Some(&background), &idle, 1100).bell_pending);
    }
}
