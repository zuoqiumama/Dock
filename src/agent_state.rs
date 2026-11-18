//! Agentic Dock state store, GUI Agent identification, zero-dep event JSON parser,
//! `--agent-event` (`WM_COPYDATA`) IPC, `%LOCALAPPDATA%\FeatherDock\agent-sessions`
//! kernel-event watcher, and zombie lifecycle cleanup.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, BOOLEAN, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, HWND,
    INVALID_HANDLE_VALUE, LPARAM, STILL_ACTIVE, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Threading::{
    CreateEventW, GetExitCodeProcess, GetProcessTimes, OpenProcess, RegisterWaitForSingleObject,
    ResetEvent, SetEvent, UnregisterWaitEx, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WT_EXECUTEONLYONCE,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, PostMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_APP, WM_COPYDATA,
};

use crate::config::ItemSpec;
use crate::dock::RunningWindowRef;
use crate::windows_list::RunningGroup;

pub const AGENT_EVENT_FLAG: &str = "--agent-event";
pub const AGENT_EVENT_COPYDATA_MAGIC: usize = 0x4644_4147; // "FDAG"
pub const WM_AGENT_SESSIONS_READY: u32 = WM_APP + 0x50;
pub const WM_AGENT_PROCESS_EXITED: u32 = WM_APP + 0x51;

const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_JSON_DEPTH: usize = 64;
const MAX_TOTAL_SESSIONS: usize = 128;
const MAX_SESSIONS_PER_AGENT: usize = 32;
const MAX_TOMBSTONES: usize = 512;
const MAX_RETIRED_PID_GENERATIONS: usize = 128;
const MAX_ID_CHARS: usize = 96;
const MAX_INSTANCE_CHARS: usize = 64;
const MAX_TITLE_CHARS: usize = 120;
const MAX_DETAIL_CHARS: usize = 240;
const MAX_PATH_CHARS: usize = 512;
const MAX_DEEP_LINK_CHARS: usize = 512;
const IPC_TIMEOUT_MS: u32 = 2000;
const WATCH_LOAD_RETRY_MS: u32 = 100;
const ALLOWED_DEEP_LINK_SCHEMES: [&str; 5] = [
    "antigravity://",
    "codex://",
    "dsh://",
    "vscode://",
    "cursor://",
];

/// Session execution state ordered from lowest (`Idle`) to highest (`Waiting`) priority
/// so `.max()` directly bubbles up the highest-priority Agent state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgentState {
    #[default]
    Idle,
    Done,
    Running,
    Error,
    Waiting,
}

impl AgentState {
    pub fn label_zh(self) -> &'static str {
        match self {
            AgentState::Waiting => "待确认",
            AgentState::Error => "异常",
            AgentState::Running => "运行中",
            AgentState::Done => "已完成",
            AgentState::Idle => "空闲",
        }
    }

    /// Stable lifecycle wording for the session card; Waiting is phrased as a
    /// question because it needs a user answer or approval rather than CPU work.
    pub fn label_en(self) -> &'static str {
        match self {
            AgentState::Waiting => "question",
            AgentState::Error => "error",
            AgentState::Running => "running",
            AgentState::Done => "done",
            AgentState::Idle => "idle",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParsedEventState {
    State(AgentState),
    Closed,
}

impl ParsedEventState {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "idle" => Some(Self::State(AgentState::Idle)),
            "done" | "completed" | "finished" => Some(Self::State(AgentState::Done)),
            "running" | "working" | "busy" => Some(Self::State(AgentState::Running)),
            "error" | "failed" => Some(Self::State(AgentState::Error)),
            "waiting" | "approval" | "confirm" | "input" => Some(Self::State(AgentState::Waiting)),
            "closed" | "exit" | "exited" | "removed" => Some(Self::Closed),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::State(AgentState::Idle) => "idle",
            Self::State(AgentState::Done) => "done",
            Self::State(AgentState::Running) => "running",
            Self::State(AgentState::Error) => "error",
            Self::State(AgentState::Waiting) => "waiting",
            Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSource {
    Window,
    Event,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSession {
    pub agent: String,
    pub session_id: String,
    pub title: String,
    pub detail: String,
    pub cwd: Option<String>,
    pub state: AgentState,
    pub hwnd: Option<isize>,
    pub pid: Option<u32>,
    pub deep_link: Option<String>,
    pub updated_at: u64,
    pub source: SessionSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEventPayload {
    pub agent: String,
    pub session_id: String,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub cwd: Option<String>,
    pub state: ParsedEventState,
    pub hwnd: Option<isize>,
    pub pid: Option<u32>,
    pub deep_link: Option<String>,
    pub updated_at: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionVersion {
    pub instance: Option<String>,
    pub seq: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDismissal {
    pub agent: String,
    pub session_id: String,
    pub version: SessionVersion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDockSummary {
    pub agent_id: String,
    pub display_name: String,
    pub aggregate_state: AgentState,
    pub total_sessions: usize,
    pub active_count: usize,
    pub waiting_count: usize,
    pub error_count: usize,
    pub done_count: usize,
    pub badge_text: Option<String>,
    pub sessions: Vec<AgentSession>,
    pub versions: HashMap<(String, String), SessionVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PidWatchRequest {
    pub pid: u32,
    agent: String,
    session_id: String,
    instance: Option<String>,
    source: SessionSource,
    seq: u64,
    updated_at: u64,
}

impl PidWatchRequest {
    fn for_session(session: &AgentSession, stamp: &VersionStamp) -> Option<Self> {
        let pid = session.pid.filter(|&pid| pid != 0)?;
        (!matches!(session.source, SessionSource::Window)).then_some(Self {
            pid,
            agent: session.agent.clone(),
            session_id: session.session_id.clone(),
            instance: stamp.instance.clone(),
            source: session.source,
            seq: stamp.seq,
            updated_at: stamp.updated_at,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventEffect {
    pub changed: bool,
    pub bounce_agent: Option<String>,
    pub watch: Option<PidWatchRequest>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventMeta {
    pub instance: Option<String>,
    pub seq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentClickAction {
    JumpSession(usize),
    TogglePopup,
    FocusOrLaunch,
}

/// Pure click routing rule shared between the main dock icon and overflow items:
/// a unique `Waiting` session (or sole active/open session) jumps straight to target;
/// multiple non-unique sessions toggle the session card.
pub fn select_agent_click_action(summary: &AgentDockSummary) -> AgentClickAction {
    if summary.waiting_count == 1 {
        if let Some(idx) = summary
            .sessions
            .iter()
            .position(|s| s.state == AgentState::Waiting)
        {
            return AgentClickAction::JumpSession(idx);
        }
    }
    if summary.sessions.len() == 1 {
        AgentClickAction::JumpSession(0)
    } else if summary.sessions.len() > 1 {
        AgentClickAction::TogglePopup
    } else {
        AgentClickAction::FocusOrLaunch
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VersionStamp {
    updated_at: u64,
    seq: u64,
    instance: Option<String>,
    pid: Option<u32>,
    proc_start: Option<u64>,
    watch_generation: Option<usize>,
    // (seq, updated_at) of the version whose OS watch observation set proc_start.
    proc_bind_version: Option<(u64, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TombstoneKind {
    Closed,
    DeadLifecycle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionTombstone {
    stamp: VersionStamp,
    kind: TombstoneKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeadHwndRecord {
    hwnd: isize,
    stamp: VersionStamp,
}

#[derive(Default)]
pub struct AgentRegistry {
    /// Presentation-only cutoff for idle history. `None` means unlimited; the
    /// stored sessions and lifecycle tombstones are never pruned by this setting.
    history_window_secs: Option<u64>,
    event_sessions: Vec<AgentSession>,
    file_sessions: Vec<AgentSession>,
    read_event_versions: HashMap<(String, String), VersionStamp>,
    read_file_versions: HashMap<(String, String), VersionStamp>,
    waiting_bounce_acked: HashSet<(String, String)>,
    event_versions: HashMap<(String, String), VersionStamp>,
    file_versions: HashMap<(String, String), VersionStamp>,
    tombstones: HashMap<(String, String), SessionTombstone>,
    dead_hwnds: HashMap<(String, String), DeadHwndRecord>,
}

fn is_older_stamp(existing: &VersionStamp, incoming: &VersionStamp) -> bool {
    match (&existing.instance, &incoming.instance) {
        (Some(ei), Some(ii)) if ei == ii => {
            if (existing.seq > 0 || incoming.seq > 0) && incoming.seq != existing.seq {
                return incoming.seq < existing.seq;
            }
            incoming.updated_at < existing.updated_at
        }
        (Some(_), Some(_)) => incoming.updated_at < existing.updated_at,
        _ => {
            if incoming.updated_at != existing.updated_at {
                return incoming.updated_at < existing.updated_at;
            }
            existing.seq > 0 && incoming.seq > 0 && incoming.seq < existing.seq
        }
    }
}

fn should_inherit_process_identity(
    existing: &VersionStamp,
    pid: Option<u32>,
    instance: Option<&str>,
    seq: u64,
    updated_at: u64,
) -> bool {
    if existing.pid.is_none() || existing.pid != pid {
        return false;
    }
    match (existing.instance.as_deref(), instance) {
        (Some(existing), Some(incoming)) => existing == incoming,
        // Legacy duplicates carry no instance proof. Preserve the process binding only
        // for the exact same event version; any newer legacy version must use the OS watch.
        (None, None) => existing.seq == seq && existing.updated_at == updated_at,
        _ => false,
    }
}

fn file_read_marker_covers(read: &VersionStamp, incoming: &VersionStamp) -> bool {
    // A read marker belongs to one producer instance. Within that instance use the
    // same seq-first ordering as live events; a different instance is always unread.
    read.instance == incoming.instance && !is_older_stamp(incoming, read)
}

fn public_version(stamp: &VersionStamp) -> SessionVersion {
    SessionVersion {
        instance: stamp.instance.clone(),
        seq: stamp.seq,
        updated_at: stamp.updated_at,
    }
}

fn newest_channel_stamp<'a>(file: &'a VersionStamp, event: &'a VersionStamp) -> &'a VersionStamp {
    // The event channel wins an exact tie, matching the previous file-then-event merge order.
    if is_older_stamp(file, event) {
        file
    } else {
        event
    }
}

fn is_blocked_by_tombstone(tomb: &SessionTombstone, incoming: &VersionStamp) -> bool {
    if is_older_stamp(&tomb.stamp, incoming) {
        return true;
    }
    match tomb.kind {
        TombstoneKind::Closed => {
            // An explicit Closed cannot be reopened by a same-second/same-seq stale packet,
            // only by a genuinely newer timestamp, higher seq on the same instance,
            // or a new instance.
            let new_instance = matches!(
                (&tomb.stamp.instance, &incoming.instance),
                (Some(old_i), Some(new_i)) if old_i != new_i
            );
            if new_instance {
                return false;
            }
            if incoming.updated_at > tomb.stamp.updated_at {
                return false;
            }
            if incoming.seq > tomb.stamp.seq {
                return false;
            }
            true
        }
        TombstoneKind::DeadLifecycle => {
            // A producer's explicit new instance is authoritative even when it restarts
            // in the same second, reuses the PID, and resets its sequence to one.
            if matches!(
                (&tomb.stamp.instance, &incoming.instance),
                (Some(old), Some(new)) if old != new
            ) {
                return false;
            }
            // An unchanged disk snapshot after process exit or dead window must not resurrect.
            let same_pid = tomb.stamp.pid.is_some()
                && incoming.pid == tomb.stamp.pid
                && match (tomb.stamp.proc_start, incoming.proc_start) {
                    (Some(old_start), Some(new_start)) => old_start == new_start,
                    _ => true,
                };
            let new_pid_restarted = tomb.stamp.pid.is_some()
                && incoming.pid.is_some()
                && !same_pid
                && incoming.updated_at >= tomb.stamp.updated_at;
            if new_pid_restarted {
                return false;
            }
            if incoming.updated_at > tomb.stamp.updated_at {
                return false;
            }
            if incoming.seq > tomb.stamp.seq {
                return false;
            }
            true
        }
    }
}

impl AgentRegistry {
    fn session_stamp(session: &AgentSession, meta: Option<&EventMeta>) -> VersionStamp {
        VersionStamp {
            updated_at: session.updated_at,
            seq: meta.and_then(|m| m.seq).unwrap_or(0),
            instance: meta
                .and_then(|m| m.instance.as_deref())
                .and_then(sanitize_instance),
            pid: session.pid.filter(|&p| p != 0),
            proc_start: None,
            watch_generation: None,
            proc_bind_version: None,
        }
    }

    fn insert_tombstone(&mut self, key: (String, String), tomb: SessionTombstone) {
        if let Some(existing) = self.tombstones.get(&key) {
            let incoming_is_older = is_older_stamp(&existing.stamp, &tomb.stamp);
            let existing_is_older = is_older_stamp(&tomb.stamp, &existing.stamp);
            if incoming_is_older
                || (!existing_is_older
                    && existing.kind == TombstoneKind::Closed
                    && tomb.kind != TombstoneKind::Closed)
            {
                return;
            }
        }
        if !self.tombstones.contains_key(&key) && self.tombstones.len() >= MAX_TOMBSTONES {
            if let Some(oldest) = self
                .tombstones
                .iter()
                .min_by_key(|(_, t)| t.stamp.updated_at)
                .map(|(k, _)| k.clone())
            {
                self.tombstones.remove(&oldest);
            }
        }
        self.tombstones.insert(key, tomb);
    }

    fn latest_stamp(&self, key: &(String, String)) -> Option<VersionStamp> {
        let file = self.file_versions.get(key).cloned().or_else(|| {
            self.file_sessions
                .iter()
                .find(|s| s.agent == key.0 && s.session_id == key.1)
                .map(|s| Self::session_stamp(s, None))
        });
        let event = self.event_versions.get(key).cloned().or_else(|| {
            self.event_sessions
                .iter()
                .find(|s| s.agent == key.0 && s.session_id == key.1)
                .map(|s| Self::session_stamp(s, None))
        });
        match (file, event) {
            (Some(file), Some(event)) => Some(newest_channel_stamp(&file, &event).clone()),
            (Some(stamp), None) | (None, Some(stamp)) => Some(stamp),
            (None, None) => None,
        }
    }

    pub fn session_version(&self, agent_id: &str, session_id: &str) -> Option<SessionVersion> {
        self.latest_stamp(&(agent_id.to_string(), session_id.to_string()))
            .as_ref()
            .map(public_version)
    }

    pub fn dismissal_record(&self, agent_id: &str, session_id: &str) -> Option<SessionDismissal> {
        Some(SessionDismissal {
            agent: agent_id.to_string(),
            session_id: session_id.to_string(),
            version: self.session_version(agent_id, session_id)?,
        })
    }

    /// Restore only user-initiated dismissals. Lifecycle tombstones remain
    /// runtime observations; these small persisted records cover Dock restarts.
    pub fn restore_dismissals(&mut self, records: Vec<SessionDismissal>) {
        for record in records {
            let Some(agent) = normalize_agent_id(&record.agent) else {
                continue;
            };
            let Some(session_id) = sanitize_id(&record.session_id, MAX_ID_CHARS) else {
                continue;
            };
            let instance = record
                .version
                .instance
                .as_deref()
                .and_then(sanitize_instance);
            self.insert_tombstone(
                (agent, session_id),
                SessionTombstone {
                    stamp: VersionStamp {
                        updated_at: record.version.updated_at,
                        seq: record.version.seq,
                        instance,
                        pid: None,
                        proc_start: None,
                        watch_generation: None,
                        proc_bind_version: None,
                    },
                    kind: TombstoneKind::Closed,
                },
            );
        }
    }

    fn channel_stamp(&self, session: &AgentSession) -> VersionStamp {
        let key = (session.agent.clone(), session.session_id.clone());
        match session.source {
            SessionSource::Event => self.event_versions.get(&key),
            SessionSource::File => self.file_versions.get(&key),
            SessionSource::Window => None,
        }
        .cloned()
        .unwrap_or_else(|| Self::session_stamp(session, None))
    }

    pub fn bind_pid_watch(
        &mut self,
        request: &PidWatchRequest,
        proc_start: Option<u64>,
        generation: usize,
    ) {
        self.bind_pid_identity(request, proc_start, Some(generation));
    }

    #[cfg(test)]
    fn bind_pid_proc_start(&mut self, request: &PidWatchRequest, proc_start: Option<u64>) {
        self.bind_pid_identity(request, proc_start, None);
    }

    fn bind_pid_identity(
        &mut self,
        request: &PidWatchRequest,
        proc_start: Option<u64>,
        watch_generation: Option<usize>,
    ) {
        if request.pid == 0 || request.source == SessionSource::Window {
            return;
        }
        let key = (request.agent.clone(), request.session_id.clone());
        let session_exists = match request.source {
            SessionSource::Event => &self.event_sessions,
            SessionSource::File => &self.file_sessions,
            SessionSource::Window => return,
        }
        .iter()
        .any(|session| {
            session.agent == request.agent
                && session.session_id == request.session_id
                && session.pid == Some(request.pid)
        });
        if !session_exists {
            return;
        }
        let stamp = match request.source {
            SessionSource::Event => self.event_versions.get_mut(&key),
            SessionSource::File => self.file_versions.get_mut(&key),
            SessionSource::Window => None,
        };
        if let Some(stamp) = stamp {
            let request_version = (request.seq, request.updated_at);
            if stamp.instance.as_deref() == request.instance.as_deref()
                && stamp.proc_bind_version != Some(request_version)
            {
                // Rebind only a genuinely new event version. An equal delayed snapshot
                // must retain its original process identity when a numeric PID is reused.
                stamp.proc_start = proc_start;
                stamp.watch_generation = watch_generation;
                stamp.proc_bind_version = Some(request_version);
            }
        }
    }

    #[cfg(test)]
    fn set_pid_proc_start(&mut self, pid: u32, proc_start: Option<u64>) {
        if pid == 0 || proc_start.is_none() {
            return;
        }
        for s in self.event_sessions.iter().chain(self.file_sessions.iter()) {
            if s.pid == Some(pid) {
                let key = (s.agent.clone(), s.session_id.clone());
                let stamp = match s.source {
                    SessionSource::Event => self.event_versions.get_mut(&key),
                    SessionSource::File => self.file_versions.get_mut(&key),
                    SessionSource::Window => None,
                };
                if let Some(stamp) = stamp {
                    if stamp.proc_start.is_none() {
                        stamp.proc_start = proc_start;
                        stamp.proc_bind_version = Some((stamp.seq, stamp.updated_at));
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub fn apply_event(
        &mut self,
        payload: AgentEventPayload,
        foreground_hwnd: Option<isize>,
        fg_agents: &[String],
    ) -> EventEffect {
        self.apply_event_with_meta(payload, EventMeta::default(), foreground_hwnd, fg_agents)
    }

    pub fn apply_event_with_meta(
        &mut self,
        payload: AgentEventPayload,
        meta: EventMeta,
        foreground_hwnd: Option<isize>,
        fg_agents: &[String],
    ) -> EventEffect {
        let Some(agent) = normalize_agent_id(&payload.agent) else {
            return EventEffect {
                changed: false,
                bounce_agent: None,
                watch: None,
            };
        };
        let Some(session_id) = sanitize_id(&payload.session_id, MAX_ID_CHARS) else {
            return EventEffect {
                changed: false,
                bounce_agent: None,
                watch: None,
            };
        };
        let key = (agent.clone(), session_id.clone());
        let existing_pos = self
            .event_sessions
            .iter()
            .position(|s| s.agent == agent && s.session_id == session_id);
        let prev = existing_pos.map(|idx| self.event_sessions[idx].clone());
        let existing_stamp = self.latest_stamp(&key);

        let incoming_ts = payload.updated_at.unwrap_or_else(now_unix_secs);
        let pid_final = payload
            .pid
            .filter(|&p| p != 0)
            .or_else(|| prev.as_ref().and_then(|p| p.pid));
        let instance = meta.instance.as_deref().and_then(sanitize_instance);
        let inherited_process = existing_stamp.as_ref().filter(|stamp| {
            should_inherit_process_identity(
                stamp,
                pid_final,
                instance.as_deref(),
                meta.seq.unwrap_or(0),
                incoming_ts,
            )
        });
        let inherited_proc_start = inherited_process.and_then(|s| s.proc_start);
        let inherited_watch_generation = inherited_process.and_then(|s| s.watch_generation);
        let inherited_proc_bind_version = inherited_process.and_then(|s| s.proc_bind_version);
        let incoming_stamp = VersionStamp {
            updated_at: incoming_ts,
            seq: meta.seq.unwrap_or(0),
            instance,
            pid: pid_final,
            proc_start: inherited_proc_start,
            watch_generation: inherited_watch_generation,
            proc_bind_version: inherited_proc_bind_version,
        };

        // Every event, including Closed, passes the same tombstone and latest-version gate
        // before it can mutate sessions or produce user-visible side effects.
        if let Some(tomb) = self.tombstones.get(&key) {
            if is_blocked_by_tombstone(tomb, &incoming_stamp) {
                return EventEffect {
                    changed: false,
                    bounce_agent: None,
                    watch: None,
                };
            }
        }
        if let Some(ref existing) = existing_stamp {
            if is_older_stamp(existing, &incoming_stamp) {
                return EventEffect {
                    changed: false,
                    bounce_agent: None,
                    watch: None,
                };
            }
        }

        let ParsedEventState::State(mut state) = payload.state else {
            self.insert_tombstone(
                key.clone(),
                SessionTombstone {
                    stamp: incoming_stamp.clone(),
                    kind: TombstoneKind::Closed,
                },
            );
            let before_event = self.event_sessions.len();
            let before_file = self.file_sessions.len();
            self.event_sessions
                .retain(|s| s.agent != agent || s.session_id != session_id);
            self.file_sessions
                .retain(|s| s.agent != agent || s.session_id != session_id);
            self.event_versions.remove(&key);
            self.file_versions.remove(&key);
            self.read_event_versions.remove(&key);
            self.read_file_versions.remove(&key);
            self.waiting_bounce_acked.remove(&key);
            self.dead_hwnds.remove(&key);
            return EventEffect {
                changed: self.event_sessions.len() != before_event
                    || self.file_sessions.len() != before_file,
                bounce_agent: None,
                watch: None,
            };
        };

        self.tombstones.remove(&key);

        let payload_hwnd = payload.hwnd.filter(|&h| h != 0);
        if payload_hwnd.is_some() {
            self.dead_hwnds.remove(&key);
        }
        let session_hwnd = payload_hwnd.or_else(|| prev.as_ref().and_then(|p| p.hwnd));
        let session_fg = is_session_foreground(&agent, session_hwnd, foreground_hwnd, fg_agents);
        if matches!(state, AgentState::Done | AgentState::Waiting) {
            let already_read = self
                .read_event_versions
                .get(&key)
                .is_some_and(|read| file_read_marker_covers(read, &incoming_stamp));
            if already_read || (state == AgentState::Done && session_fg) {
                state = AgentState::Idle;
                self.read_event_versions
                    .insert(key.clone(), incoming_stamp.clone());
            }
        } else {
            self.read_event_versions.remove(&key);
        }
        if state != AgentState::Waiting {
            self.waiting_bounce_acked.remove(&key);
        }

        let cwd = payload
            .cwd
            .as_deref()
            .and_then(sanitize_path_value)
            .or_else(|| prev.as_ref().and_then(|p| p.cwd.clone()));
        let title = payload
            .title
            .as_deref()
            .and_then(|t| sanitize_text(t, MAX_TITLE_CHARS))
            .or_else(|| cwd.as_deref().and_then(cwd_leaf_name))
            .or_else(|| prev.as_ref().map(|p| p.title.clone()))
            .unwrap_or_else(|| display_name(&agent));
        let detail = payload
            .detail
            .as_deref()
            .and_then(|d| sanitize_text(d, MAX_DETAIL_CHARS))
            .or_else(|| prev.as_ref().map(|p| p.detail.clone()))
            .unwrap_or_default();
        let hwnd = session_hwnd;
        let pid = incoming_stamp.pid;
        let deep_link = payload
            .deep_link
            .as_deref()
            .and_then(validate_deep_link)
            .or_else(|| prev.as_ref().and_then(|p| p.deep_link.clone()));
        let updated_at = incoming_ts;

        let updated = AgentSession {
            agent: agent.clone(),
            session_id: session_id.clone(),
            title,
            detail,
            cwd,
            state,
            hwnd,
            pid,
            deep_link,
            updated_at,
            source: SessionSource::Event,
        };

        let watch = PidWatchRequest::for_session(&updated, &incoming_stamp);
        let mut changed = true;
        if let Some(idx) = existing_pos {
            changed = self.event_sessions[idx] != updated;
            self.event_sessions[idx] = updated;
        } else {
            self.evict_if_needed(&agent);
            self.event_sessions.push(updated);
        }
        self.event_versions.insert(key.clone(), incoming_stamp);

        let newly_waiting = state == AgentState::Waiting
            && prev.as_ref().is_none_or(|p| p.state != AgentState::Waiting);
        let bounce_agent = if newly_waiting && !session_fg && self.waiting_bounce_acked.insert(key)
        {
            Some(agent)
        } else {
            None
        };

        EventEffect {
            changed,
            bounce_agent,
            watch,
        }
    }

    fn evict_if_needed(&mut self, agent: &str) {
        let agent_count = self
            .event_sessions
            .iter()
            .filter(|s| s.agent == agent)
            .count();
        if agent_count >= MAX_SESSIONS_PER_AGENT {
            if let Some(pos) = self
                .event_sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| s.agent == agent)
                .min_by_key(|(_, s)| (s.state, s.updated_at))
                .map(|(i, _)| i)
            {
                self.event_sessions.remove(pos);
            }
        }
        if self.event_sessions.len() >= MAX_TOTAL_SESSIONS {
            if let Some(pos) = self
                .event_sessions
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| (s.state, s.updated_at))
                .map(|(i, _)| i)
            {
                self.event_sessions.remove(pos);
            }
        }
    }

    #[cfg(test)]
    pub fn replace_file_sessions(
        &mut self,
        incoming: Vec<AgentSession>,
        foreground_hwnd: Option<isize>,
        fg_agents: &[String],
    ) -> (bool, Vec<String>, Vec<PidWatchRequest>) {
        let with_meta = incoming
            .into_iter()
            .map(|s| (s, EventMeta::default()))
            .collect();
        self.replace_file_sessions_with_meta(with_meta, foreground_hwnd, fg_agents)
    }

    pub fn replace_file_sessions_with_meta(
        &mut self,
        incoming: Vec<(AgentSession, EventMeta)>,
        foreground_hwnd: Option<isize>,
        fg_agents: &[String],
    ) -> (bool, Vec<String>, Vec<PidWatchRequest>) {
        let old = std::mem::replace(&mut self.file_sessions, Vec::with_capacity(incoming.len()));
        let mut bounces = Vec::new();
        let mut watch_requests = Vec::new();
        let mut incoming_keys = HashSet::with_capacity(incoming.len());

        for (mut session, meta) in incoming {
            let key = (session.agent.clone(), session.session_id.clone());
            incoming_keys.insert(key.clone());
            let mut stamp = Self::session_stamp(&session, Some(&meta));

            if let Some(tomb) = self.tombstones.get(&key) {
                if is_blocked_by_tombstone(tomb, &stamp) {
                    continue;
                }
            }

            let prev = old
                .iter()
                .find(|s| s.agent == session.agent && s.session_id == session.session_id);
            let event = self
                .event_sessions
                .iter()
                .find(|session| session.agent == key.0 && session.session_id == key.1)
                .map(|session| {
                    let stamp = self
                        .event_versions
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| Self::session_stamp(session, None));
                    (session, stamp)
                });
            let previous_visible_state = match (event.as_ref(), prev) {
                (Some((event, event_stamp)), Some(file)) => {
                    let file_stamp = self
                        .file_versions
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| Self::session_stamp(file, None));
                    if is_older_stamp(&file_stamp, event_stamp) {
                        file.state
                    } else {
                        event.state
                    }
                }
                (Some((event, _)), None) => event.state,
                (None, Some(file)) => file.state,
                (None, None) => AgentState::Idle,
            };
            if let Some((_, event_stamp)) = event.as_ref() {
                // `is_older_stamp(&stamp, event_stamp)` is true precisely when the file is
                // strictly newer. Equal versions belong to IPC by deterministic tie-break.
                if !is_older_stamp(&stamp, event_stamp) {
                    // The hidden file channel still owns its read/ack and process identity.
                    // Preserve its last accepted row without applying delayed side effects.
                    if let Some(previous) = prev {
                        // The newer IPC channel owns the current OS watch request. Keep the
                        // hidden file row/metadata without rebinding it to that process.
                        self.file_sessions.push(previous.clone());
                    }
                    continue;
                }
            }

            if let Some(existing) = self
                .file_versions
                .get(&key)
                .cloned()
                .or_else(|| prev.map(|session| Self::session_stamp(session, None)))
            {
                if is_older_stamp(&existing, &stamp) {
                    // A delayed directory snapshot must not roll back this channel. Keep the
                    // last accepted row and its original version stamp.
                    if let Some(previous) = prev {
                        if let Some(request) = PidWatchRequest::for_session(previous, &existing) {
                            watch_requests.push(request);
                        }
                        self.file_sessions.push(previous.clone());
                    }
                    continue;
                }
            }

            self.tombstones.remove(&key);
            if let Some((prev_start, prev_generation, prev_bind_version)) = self
                .file_versions
                .get(&key)
                .filter(|prev| {
                    should_inherit_process_identity(
                        prev,
                        stamp.pid,
                        stamp.instance.as_deref(),
                        stamp.seq,
                        stamp.updated_at,
                    )
                })
                .map(|prev| {
                    (
                        prev.proc_start,
                        prev.watch_generation,
                        prev.proc_bind_version,
                    )
                })
            {
                stamp.proc_start = prev_start;
                stamp.watch_generation = prev_generation;
                stamp.proc_bind_version = prev_bind_version;
            }

            if let Some(dead) = self.dead_hwnds.get(&key) {
                if session.hwnd == Some(dead.hwnd) && !is_older_stamp(&stamp, &dead.stamp) {
                    session.hwnd = None;
                }
            }

            let session_fg =
                is_session_foreground(&session.agent, session.hwnd, foreground_hwnd, fg_agents);
            if matches!(session.state, AgentState::Done | AgentState::Waiting) {
                let already_read = self
                    .read_file_versions
                    .get(&key)
                    .is_some_and(|read| file_read_marker_covers(read, &stamp));
                if already_read
                    || (session.state == AgentState::Done
                        && session_fg
                        && prev.is_none_or(|p| p.state != AgentState::Done))
                {
                    session.state = AgentState::Idle;
                    self.read_file_versions.insert(key.clone(), stamp.clone());
                }
            } else {
                self.read_file_versions.remove(&key);
            }

            if session.state != AgentState::Waiting {
                self.waiting_bounce_acked.remove(&key);
            } else if previous_visible_state != AgentState::Waiting
                && !session_fg
                && self.waiting_bounce_acked.insert(key.clone())
            {
                // Compare against the previously visible merged channel, not a hidden old
                // file row. IPC Running in between must permit exactly one new bounce.
                bounces.push(session.agent.clone());
            }

            if let Some(request) = PidWatchRequest::for_session(&session, &stamp) {
                watch_requests.push(request);
            }
            self.file_versions.insert(key, stamp);
            self.file_sessions.push(session);
        }

        let live_keys: HashSet<(String, String)> = self
            .file_sessions
            .iter()
            .map(|s| (s.agent.clone(), s.session_id.clone()))
            .collect();
        self.file_versions.retain(|key, _| live_keys.contains(key));
        self.read_file_versions
            .retain(|key, _| live_keys.contains(key));
        self.waiting_bounce_acked.retain(|key| {
            live_keys.contains(key)
                || self
                    .event_sessions
                    .iter()
                    .any(|s| s.agent == key.0 && s.session_id == key.1)
        });
        self.dead_hwnds
            .retain(|key, _| incoming_keys.contains(key) || live_keys.contains(key));
        // Confirmed file deletion removes the live row, not its lifecycle tombstone.
        // Tombstones are released only by a genuinely newer event or bounded LRU eviction.

        let changed = old != self.file_sessions;
        (changed, bounces, watch_requests)
    }

    pub fn on_foreground_changed(
        &mut self,
        foreground_hwnd: Option<isize>,
        fg_agents: &[String],
    ) -> bool {
        let mut changed = false;
        for s in &mut self.event_sessions {
            if is_session_foreground(&s.agent, s.hwnd, foreground_hwnd, fg_agents) {
                if s.state == AgentState::Done {
                    s.state = AgentState::Idle;
                    changed = true;
                }
                if s.state == AgentState::Waiting {
                    self.waiting_bounce_acked
                        .insert((s.agent.clone(), s.session_id.clone()));
                }
            }
        }
        for s in &mut self.file_sessions {
            if is_session_foreground(&s.agent, s.hwnd, foreground_hwnd, fg_agents) {
                if s.state == AgentState::Done {
                    let key = (s.agent.clone(), s.session_id.clone());
                    let stamp = self
                        .file_versions
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| Self::session_stamp(s, None));
                    s.state = AgentState::Idle;
                    self.read_file_versions.insert(key, stamp);
                    changed = true;
                }
                if s.state == AgentState::Waiting {
                    self.waiting_bounce_acked
                        .insert((s.agent.clone(), s.session_id.clone()));
                }
            }
        }
        changed
    }

    pub fn mark_agent_read(&mut self, agent_id: &str) -> bool {
        let mut changed = false;
        for s in &mut self.event_sessions {
            if s.agent == agent_id && s.state == AgentState::Done {
                s.state = AgentState::Idle;
                changed = true;
            }
            if s.agent == agent_id && s.state == AgentState::Waiting {
                self.waiting_bounce_acked
                    .insert((s.agent.clone(), s.session_id.clone()));
            }
        }
        for s in &mut self.file_sessions {
            if s.agent == agent_id && s.state == AgentState::Done {
                let key = (s.agent.clone(), s.session_id.clone());
                let stamp = self
                    .file_versions
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| Self::session_stamp(s, None));
                s.state = AgentState::Idle;
                self.read_file_versions.insert(key, stamp);
                changed = true;
            }
            if s.agent == agent_id && s.state == AgentState::Waiting {
                self.waiting_bounce_acked
                    .insert((s.agent.clone(), s.session_id.clone()));
            }
        }
        changed
    }

    pub fn mark_session_read(&mut self, agent_id: &str, session_id: &str) -> bool {
        let mut changed = false;
        for s in &mut self.event_sessions {
            if s.agent == agent_id && s.session_id == session_id {
                if s.state == AgentState::Done {
                    s.state = AgentState::Idle;
                    changed = true;
                }
                if s.state == AgentState::Waiting {
                    self.waiting_bounce_acked
                        .insert((s.agent.clone(), s.session_id.clone()));
                }
            }
        }
        for s in &mut self.file_sessions {
            if s.agent == agent_id && s.session_id == session_id {
                if s.state == AgentState::Done {
                    let key = (s.agent.clone(), s.session_id.clone());
                    let stamp = self
                        .file_versions
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| Self::session_stamp(s, None));
                    s.state = AgentState::Idle;
                    self.read_file_versions.insert(key, stamp);
                    changed = true;
                }
                if s.state == AgentState::Waiting {
                    self.waiting_bounce_acked
                        .insert((s.agent.clone(), s.session_id.clone()));
                }
            }
        }
        changed
    }

    /// Explicitly acknowledge the current Waiting/Done notification while
    /// retaining its session as an Idle history row. Read markers are scoped to
    /// each producer version, so a newer Waiting event or a new instance alerts
    /// again, while an unchanged IPC packet or file reload stays acknowledged.
    pub fn acknowledge_session(&mut self, agent_id: &str, session_id: &str) -> bool {
        let mut changed = false;
        let key = (agent_id.to_string(), session_id.to_string());
        for session in &mut self.event_sessions {
            if session.agent == agent_id
                && session.session_id == session_id
                && matches!(session.state, AgentState::Waiting | AgentState::Done)
            {
                let stamp = self
                    .event_versions
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| Self::session_stamp(session, None));
                session.state = AgentState::Idle;
                self.read_event_versions.insert(key.clone(), stamp);
                self.waiting_bounce_acked.remove(&key);
                changed = true;
            }
        }
        for session in &mut self.file_sessions {
            if session.agent == agent_id
                && session.session_id == session_id
                && matches!(session.state, AgentState::Waiting | AgentState::Done)
            {
                let stamp = self
                    .file_versions
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| Self::session_stamp(session, None));
                session.state = AgentState::Idle;
                self.read_file_versions.insert(key.clone(), stamp);
                self.waiting_bounce_acked.remove(&key);
                changed = true;
            }
        }
        changed
    }

    /// Remove a Track B session locally and protect the dismissal with the
    /// latest observed version. Returns PIDs that no remaining session uses,
    /// allowing the caller to release their OS wait registrations as well.
    pub fn dismiss_session(&mut self, agent_id: &str, session_id: &str) -> (bool, Vec<u32>) {
        let key = (agent_id.to_string(), session_id.to_string());
        let Some(stamp) = self.latest_stamp(&key) else {
            return (false, Vec::new());
        };
        let watched_pids: HashSet<u32> = self
            .event_sessions
            .iter()
            .chain(self.file_sessions.iter())
            .filter(|session| session.agent == agent_id && session.session_id == session_id)
            .filter_map(|session| session.pid)
            .filter(|&pid| pid != 0)
            .collect();
        self.insert_tombstone(
            key.clone(),
            SessionTombstone {
                stamp,
                kind: TombstoneKind::Closed,
            },
        );
        let before_event = self.event_sessions.len();
        let before_file = self.file_sessions.len();
        self.event_sessions
            .retain(|session| session.agent != agent_id || session.session_id != session_id);
        self.file_sessions
            .retain(|session| session.agent != agent_id || session.session_id != session_id);
        self.event_versions.remove(&key);
        self.file_versions.remove(&key);
        self.read_event_versions.remove(&key);
        self.read_file_versions.remove(&key);
        self.waiting_bounce_acked.remove(&key);
        self.dead_hwnds.remove(&key);

        let mut released_pids: Vec<u32> = watched_pids
            .into_iter()
            .filter(|pid| {
                !self
                    .event_sessions
                    .iter()
                    .chain(self.file_sessions.iter())
                    .any(|session| session.pid == Some(*pid))
            })
            .collect();
        released_pids.sort_unstable();
        (
            self.event_sessions.len() != before_event || self.file_sessions.len() != before_file,
            released_pids,
        )
    }

    pub fn prune_dead_windows<F>(&mut self, mut is_alive: F) -> bool
    where
        F: FnMut(isize) -> bool,
    {
        let mut changed = false;
        let mut drop_keys: Vec<((String, String), VersionStamp)> = Vec::new();
        let mut new_dead_hwnds: Vec<((String, String), DeadHwndRecord)> = Vec::new();

        for list in [&mut self.event_sessions, &mut self.file_sessions] {
            let mut idx = 0usize;
            while idx < list.len() {
                let Some(hwnd) = list[idx].hwnd else {
                    idx += 1;
                    continue;
                };
                if is_alive(hwnd) {
                    idx += 1;
                    continue;
                }
                let key = (list[idx].agent.clone(), list[idx].session_id.clone());
                let stamp = match list[idx].source {
                    SessionSource::Event => self.event_versions.get(&key),
                    SessionSource::File => self.file_versions.get(&key),
                    SessionSource::Window => None,
                }
                .cloned()
                .unwrap_or_else(|| Self::session_stamp(&list[idx], None));
                let has_live_process_evidence =
                    list[idx].pid.filter(|&p| p != 0).is_some() && stamp.proc_start.is_some();
                if has_live_process_evidence {
                    list[idx].hwnd = None;
                    new_dead_hwnds.push((key, DeadHwndRecord { hwnd, stamp }));
                    changed = true;
                    idx += 1;
                } else {
                    list.remove(idx);
                    drop_keys.push((key, stamp));
                    changed = true;
                }
            }
        }

        for (key, rec) in new_dead_hwnds {
            self.dead_hwnds.insert(key, rec);
        }
        for (key, stamp) in drop_keys {
            let event_alive = self
                .event_sessions
                .iter()
                .any(|s| s.agent == key.0 && s.session_id == key.1);
            let file_alive = self
                .file_sessions
                .iter()
                .any(|s| s.agent == key.0 && s.session_id == key.1);
            if !event_alive && !file_alive {
                self.waiting_bounce_acked.remove(&key);
            }
            if !file_alive {
                self.read_file_versions.remove(&key);
                self.file_versions.remove(&key);
            }
            if !event_alive {
                self.read_event_versions.remove(&key);
                self.event_versions.remove(&key);
            }
            self.insert_tombstone(
                key,
                SessionTombstone {
                    stamp,
                    kind: TombstoneKind::DeadLifecycle,
                },
            );
        }
        changed
    }

    #[cfg(test)]
    pub fn prune_pid(&mut self, pid: u32) -> bool {
        self.prune_pid_with_start(pid, None)
    }

    pub fn prune_pid_with_start(&mut self, pid: u32, proc_start: Option<u64>) -> bool {
        self.prune_pid_with_identity(pid, proc_start, None)
    }

    fn prune_pid_with_generation(&mut self, pid: u32, generation: usize) -> bool {
        self.prune_pid_with_identity(pid, None, Some(generation))
    }

    fn prune_pid_with_identity(
        &mut self,
        pid: u32,
        proc_start: Option<u64>,
        watch_generation: Option<usize>,
    ) -> bool {
        if pid == 0 {
            return false;
        }
        let mut changed = false;
        let mut tombstones: Vec<((String, String), VersionStamp)> = Vec::new();
        for list in [&mut self.event_sessions, &mut self.file_sessions] {
            let mut idx = 0usize;
            while idx < list.len() {
                if list[idx].pid != Some(pid) {
                    idx += 1;
                    continue;
                }
                let key = (list[idx].agent.clone(), list[idx].session_id.clone());
                let stamp = match list[idx].source {
                    SessionSource::Event => self.event_versions.get(&key),
                    SessionSource::File => self.file_versions.get(&key),
                    SessionSource::Window => None,
                }
                .cloned()
                .unwrap_or_else(|| Self::session_stamp(&list[idx], None));
                if let Some(expected) = watch_generation {
                    // Unknown OS start times still have an exact in-process watch identity.
                    if stamp.watch_generation != Some(expected) {
                        idx += 1;
                        continue;
                    }
                } else if let Some(expected) = proc_start {
                    // A known exited generation must not remove a newer or not-yet-bound
                    // process instance that happens to reuse the same numeric PID.
                    if stamp.proc_start != Some(expected) {
                        idx += 1;
                        continue;
                    }
                }
                list.remove(idx);
                tombstones.push((key, stamp));
                changed = true;
            }
        }
        for (key, stamp) in tombstones {
            let event_alive = self
                .event_sessions
                .iter()
                .any(|s| s.agent == key.0 && s.session_id == key.1);
            let file_alive = self
                .file_sessions
                .iter()
                .any(|s| s.agent == key.0 && s.session_id == key.1);
            if !event_alive && !file_alive {
                self.waiting_bounce_acked.remove(&key);
                self.dead_hwnds.remove(&key);
            }
            if !file_alive {
                self.read_file_versions.remove(&key);
                self.file_versions.remove(&key);
            }
            if !event_alive {
                self.read_event_versions.remove(&key);
                self.event_versions.remove(&key);
            }
            self.insert_tombstone(
                key,
                SessionTombstone {
                    stamp,
                    kind: TombstoneKind::DeadLifecycle,
                },
            );
        }
        changed
    }

    /// Set the presentation-only cutoff for idle history. Active sessions,
    /// unread completion reminders and Track A windows are always visible.
    pub fn set_history_window(&mut self, history_window_secs: Option<u64>) {
        self.history_window_secs = history_window_secs;
    }

    /// Agent IDs that currently have active Track B sessions (`state != Idle` or non-empty)
    /// so an Agent running via events/hooks can appear even before or without a pinned slot.
    pub fn track_b_agent_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for s in self.event_sessions.iter().chain(self.file_sessions.iter()) {
            if !ids.iter().any(|existing| existing == &s.agent) {
                ids.push(s.agent.clone());
            }
        }
        ids
    }

    /// Merge Track B sessions (files + IPC events) with Track A GUI windows for one Agent.
    pub fn summarize_agent(
        &self,
        agent_id: &str,
        display_label: &str,
        windows: &[RunningWindowRef],
    ) -> AgentDockSummary {
        let mut merged: Vec<(AgentSession, VersionStamp)> = Vec::new();
        for session in self
            .file_sessions
            .iter()
            .chain(self.event_sessions.iter())
            .filter(|s| s.agent == agent_id)
        {
            let key = (session.agent.clone(), session.session_id.clone());
            let stamp = self.channel_stamp(session);
            if self
                .tombstones
                .get(&key)
                .is_some_and(|tomb| is_blocked_by_tombstone(tomb, &stamp))
            {
                continue;
            }
            if let Some(pos) = merged
                .iter()
                .position(|(existing, _)| existing.session_id == session.session_id)
            {
                if !is_older_stamp(&merged[pos].1, &stamp) {
                    merged[pos] = (session.clone(), stamp);
                }
            } else {
                merged.push((session.clone(), stamp));
            }
        }

        // Keep lifecycle storage intact while hiding only stale idle history from
        // summaries, badges and popup actions. A later event can make the same
        // session visible again without resurrecting a dismissed/closed identity.
        if let Some(window_secs) = self.history_window_secs {
            let now = now_unix_secs();
            merged.retain(|(session, _)| {
                session.source == SessionSource::Window
                    || session.state != AgentState::Idle
                    || session.updated_at == 0
                    || now.saturating_sub(session.updated_at) <= window_secs
            });
        }

        let versions = merged
            .iter()
            .map(|(session, stamp)| {
                (
                    (session.agent.clone(), session.session_id.clone()),
                    public_version(stamp),
                )
            })
            .collect();
        let mut merged: Vec<AgentSession> =
            merged.into_iter().map(|(session, _)| session).collect();

        let has_track_b = !merged.is_empty();
        let default_hwnd = windows.first().map(|w| w.hwnd);
        let covered_hwnds: HashSet<isize> = merged.iter().filter_map(|s| s.hwnd).collect();
        for s in &mut merged {
            if s.hwnd.is_none() {
                s.hwnd = default_hwnd;
            }
        }

        // Track A fallback rows:
        // - If there are NO Track B sessions, represent every GUI window.
        // - If there ARE Track B sessions, only add uncovered windows when multiple GUI
        //   windows exist so multi-window Agents (e.g. Antigravity workspaces) show all
        //   workspaces without duplicating the outer shell window of single-window Agents.
        let mut track_a_count = 0usize;
        if !has_track_b || windows.len() > 1 {
            for win in windows {
                if !covered_hwnds.contains(&win.hwnd) {
                    track_a_count += 1;
                    merged.push(AgentSession {
                        agent: agent_id.to_string(),
                        session_id: format!("hwnd:{}", win.hwnd),
                        title: win.title.clone(),
                        detail: "窗口已打开 · 未接入任务事件".to_string(),
                        cwd: None,
                        state: AgentState::Idle,
                        hwnd: Some(win.hwnd),
                        pid: None,
                        deep_link: None,
                        updated_at: 0,
                        source: SessionSource::Window,
                    });
                }
            }
        }

        merged.sort_by(|a, b| {
            b.state
                .cmp(&a.state)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| a.title.cmp(&b.title))
        });

        let aggregate_state = merged
            .iter()
            .map(|s| s.state)
            .max()
            .unwrap_or(AgentState::Idle);
        let waiting_count = merged
            .iter()
            .filter(|s| s.state == AgentState::Waiting)
            .count();
        let error_count = merged
            .iter()
            .filter(|s| s.state == AgentState::Error)
            .count();
        let running_count = merged
            .iter()
            .filter(|s| s.state == AgentState::Running)
            .count();
        let done_count = merged
            .iter()
            .filter(|s| s.state == AgentState::Done)
            .count();
        let active_count = running_count + error_count;
        let total_sessions = merged.len();

        let badge_text = if waiting_count > 0 {
            if total_sessions > 1 || waiting_count > 1 {
                Some(format!("{waiting_count}!"))
            } else {
                Some("!".to_string())
            }
        } else if active_count > 0 && has_track_b {
            Some(active_count.to_string())
        } else if windows.len() > 1 {
            Some(windows.len().to_string())
        } else if total_sessions > 1 && track_a_count != 1 {
            Some(total_sessions.to_string())
        } else {
            None
        };

        let display_name = if display_label.trim().is_empty() {
            display_name(agent_id)
        } else {
            display_label.to_string()
        };

        AgentDockSummary {
            agent_id: agent_id.to_string(),
            display_name,
            aggregate_state,
            total_sessions,
            active_count,
            waiting_count,
            error_count,
            done_count,
            badge_text,
            sessions: merged,
            versions,
        }
    }
}

fn is_session_foreground(
    agent_id: &str,
    session_hwnd: Option<isize>,
    foreground_hwnd: Option<isize>,
    fg_agents: &[String],
) -> bool {
    let Some(fg) = foreground_hwnd.filter(|&h| h != 0) else {
        return false;
    };
    match session_hwnd.filter(|&h| h != 0) {
        Some(hwnd) => hwnd == fg,
        None => fg_agents.iter().any(|a| a == agent_id),
    }
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn normalize_agent_id(raw: &str) -> Option<String> {
    let mut out = String::new();
    for ch in raw.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || matches!(lower, '-' | '_') {
            out.push(lower);
            if out.len() >= 48 {
                break;
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

pub fn display_name(agent_id: &str) -> String {
    match agent_id {
        "antigravity" => "Antigravity".to_string(),
        "codex" => "Codex".to_string(),
        "dsh" => "dsh".to_string(),
        other => other.to_string(),
    }
}

pub fn fallback_visual(agent_id: &str) -> (&'static str, (f32, f32, f32)) {
    match agent_id {
        "antigravity" => ("\u{2728}", (0.28, 0.56, 0.52)),
        "codex" => ("\u{26A1}", (0.26, 0.44, 0.62)),
        "dsh" => ("\u{1F4BB}", (0.38, 0.46, 0.56)),
        _ => ("\u{2726}", (0.41, 0.53, 0.47)),
    }
}

fn token_matches(text: &str, needle: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|part| part.eq_ignore_ascii_case(needle))
}

fn stem_matches_agent(stem_lower: &str, id: &str) -> bool {
    stem_lower == id
        || stem_lower.starts_with(&format!("{id}-"))
        || stem_lower.starts_with(&format!("{id}_"))
        || stem_lower.starts_with(&format!("{id} "))
}

pub fn infer_agent_from_exe_or_aumid(
    exe_path: Option<&str>,
    app_id: Option<&str>,
) -> Option<String> {
    if let Some(aumid) = app_id {
        let lower = aumid.to_ascii_lowercase();
        let trimmed = lower.strip_prefix("aumid:").unwrap_or(&lower);
        if token_matches(trimmed, "antigravity") {
            return Some("antigravity".to_string());
        }
        if trimmed.contains("openai.codex") || token_matches(trimmed, "codex") {
            return Some("codex".to_string());
        }
        if token_matches(trimmed, "dsh") {
            return Some("dsh".to_string());
        }
    }
    if let Some(path) = exe_path {
        let normalized = path.replace('/', "\\").to_ascii_lowercase();
        let stem = Path::new(&normalized)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if stem_matches_agent(stem, "antigravity") || normalized.contains("\\antigravity\\") {
            return Some("antigravity".to_string());
        }
        if stem_matches_agent(stem, "codex")
            || normalized.contains("\\openai\\codex\\")
            || normalized.contains("\\windowsapps\\openai.codex_")
        {
            return Some("codex".to_string());
        }
        if stem_matches_agent(stem, "dsh") || normalized.contains("\\.dsh-purge-pocket\\") {
            return Some("dsh".to_string());
        }
    }
    None
}

pub fn infer_agent_id_for_spec(spec: &ItemSpec, resolved_path: Option<&str>) -> Option<String> {
    let explicit_agent = spec.agent.as_deref().and_then(normalize_agent_id);
    let explicit_kind = spec
        .kind
        .as_deref()
        .is_some_and(|k| k.trim().eq_ignore_ascii_case("agent"));
    if explicit_agent.is_some() {
        return explicit_agent;
    }
    if explicit_kind {
        if let Some(inferred) =
            infer_agent_from_exe_or_aumid(resolved_path.or(spec.path.as_deref()), None)
        {
            return Some(inferred);
        }
        if let Some(label) = spec.label.as_deref().and_then(normalize_agent_id) {
            return Some(label);
        }
        return Some("agent".to_string());
    }
    // Auto-recognize Antigravity / Codex / dsh even if user pinned them without `kind = "agent"`.
    infer_agent_from_exe_or_aumid(resolved_path.or(spec.path.as_deref()), spec.app.as_deref())
}

pub fn infer_agent_id_for_group(group: &RunningGroup) -> Option<String> {
    if let Some(found) = infer_agent_from_exe_or_aumid(
        group.icon_path.as_deref(),
        group
            .key
            .starts_with("aumid:")
            .then_some(group.key.as_str()),
    ) {
        return Some(found);
    }
    for window in &group.windows {
        if let Some(found) =
            infer_agent_from_exe_or_aumid(window.exe_path.as_deref(), window.app_id.as_deref())
        {
            return Some(found);
        }
    }
    None
}

fn sanitize_text(raw: &str, max_chars: usize) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect();
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn sanitize_id(raw: &str, max_chars: usize) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect();
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn sanitize_instance(raw: &str) -> Option<String> {
    sanitize_id(raw, MAX_INSTANCE_CHARS)
}

fn sanitize_path_value(raw: &str) -> Option<String> {
    sanitize_text(raw, MAX_PATH_CHARS)
}

fn cwd_leaf_name(cwd: &str) -> Option<String> {
    let trimmed = cwd.trim_end_matches(['\\', '/']);
    Path::new(trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|s| sanitize_text(s, MAX_TITLE_CHARS))
}

pub fn validate_deep_link(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_DEEP_LINK_CHARS {
        return None;
    }
    if trimmed.chars().any(|c| {
        c.is_control() || c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | '|' | ';')
    }) {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if ALLOWED_DEEP_LINK_SCHEMES
        .iter()
        .any(|scheme| lower.starts_with(scheme) && lower.len() > scheme.len())
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

pub fn sessions_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("FeatherDock")
        .join("agent-sessions")
}

#[cfg(test)]
pub fn load_sessions_from_dir(dir: &Path) -> Vec<AgentSession> {
    load_sessions_preserving(dir)
        .map(|(_, sessions)| sessions.into_iter().map(|(s, _)| s).collect())
        .unwrap_or_default()
}

pub(crate) type CachedFileMap = HashMap<PathBuf, (AgentSession, EventMeta)>;
pub(crate) type SessionSnapshot = (CachedFileMap, Vec<(AgentSession, EventMeta)>);

pub(crate) fn load_sessions_preserving(dir: &Path) -> std::io::Result<SessionSnapshot> {
    load_sessions_preserving_with(dir, |path| fs::read_to_string(path))
}

fn load_sessions_preserving_with<F>(
    dir: &Path,
    mut read_file: F,
) -> std::io::Result<(CachedFileMap, Vec<(AgentSession, EventMeta)>)>
where
    F: FnMut(&Path) -> std::io::Result<String>,
{
    let entries = fs::read_dir(dir)?;
    let mut next_cache: CachedFileMap = HashMap::new();
    let mut out = Vec::new();
    let now = now_unix_secs();
    for entry in entries {
        let entry = entry?;
        if out.len() >= MAX_TOTAL_SESSIONS {
            break;
        }
        let path = entry.path();
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_none_or(|e| !e.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        // A snapshot containing an unreadable live file is not authoritative. Return
        // the error so the watcher retains its previous complete cache/snapshot and
        // suppresses READY instead of treating that file as deleted.
        let meta = entry.metadata()?;
        if !meta.is_file() || meta.len() > MAX_PAYLOAD_BYTES as u64 {
            continue;
        }
        let text = read_file(&path)?;
        let Some((payload, event_meta)) = parse_agent_event_json_with_meta(&text) else {
            continue;
        };
        let ParsedEventState::State(state) = payload.state else {
            continue;
        };
        let Some(agent) = normalize_agent_id(&payload.agent) else {
            continue;
        };
        let Some(session_id) = sanitize_id(&payload.session_id, MAX_ID_CHARS) else {
            continue;
        };
        let cwd = payload.cwd.as_deref().and_then(sanitize_path_value);
        let title = payload
            .title
            .as_deref()
            .and_then(|t| sanitize_text(t, MAX_TITLE_CHARS))
            .or_else(|| cwd.as_deref().and_then(cwd_leaf_name))
            .unwrap_or_else(|| display_name(&agent));
        let detail = payload
            .detail
            .as_deref()
            .and_then(|d| sanitize_text(d, MAX_DETAIL_CHARS))
            .unwrap_or_default();
        let file_ts = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(now);
        let entry_val = (
            AgentSession {
                agent,
                session_id,
                title,
                detail,
                cwd,
                state,
                hwnd: payload.hwnd.filter(|&h| h != 0),
                pid: payload.pid.filter(|&p| p != 0),
                deep_link: payload.deep_link.as_deref().and_then(validate_deep_link),
                updated_at: payload.updated_at.unwrap_or(file_ts),
                source: SessionSource::File,
            },
            event_meta,
        );
        next_cache.insert(path, entry_val.clone());
        out.push(entry_val);
    }
    Ok((next_cache, out))
}

pub fn format_relative_age(updated_at: u64, now: u64) -> String {
    if updated_at == 0 || now <= updated_at {
        return "刚刚".to_string();
    }
    let delta = now - updated_at;
    if delta < 15 {
        "刚刚".to_string()
    } else if delta < 60 {
        format!("{delta}s")
    } else if delta < 3600 {
        let mins = delta / 60;
        let secs = delta % 60;
        if secs == 0 {
            format!("{mins}m")
        } else {
            format!("{mins}m{secs}s")
        }
    } else if delta < 86_400 {
        format!("{}h", delta / 3600)
    } else {
        format!("{}d", delta / 86_400)
    }
}

// ---------- Zero-dependency JSON payload parser & serializer ----------

pub fn serialize_event_json_with_meta(payload: &AgentEventPayload, meta: &EventMeta) -> String {
    let mut out = String::from("{");
    push_json_kv(&mut out, "agent", &payload.agent, true);
    push_json_kv(&mut out, "session_id", &payload.session_id, false);
    push_json_kv(&mut out, "state", payload.state.as_str(), false);
    if let Some(title) = &payload.title {
        push_json_kv(&mut out, "title", title, false);
    }
    if let Some(detail) = &payload.detail {
        push_json_kv(&mut out, "detail", detail, false);
    }
    if let Some(cwd) = &payload.cwd {
        push_json_kv(&mut out, "cwd", cwd, false);
    }
    if let Some(hwnd) = payload.hwnd {
        out.push_str(&format!(",\"hwnd\":{hwnd}"));
    }
    if let Some(pid) = payload.pid {
        out.push_str(&format!(",\"pid\":{pid}"));
    }
    if let Some(link) = &payload.deep_link {
        push_json_kv(&mut out, "deep_link", link, false);
    }
    if let Some(ts) = payload.updated_at {
        out.push_str(&format!(",\"updated_at\":{ts}"));
    }
    if let Some(inst) = &meta.instance {
        push_json_kv(&mut out, "instance", inst, false);
    }
    if let Some(seq) = meta.seq {
        out.push_str(&format!(",\"seq\":{seq}"));
    }
    out.push('}');
    out
}

fn push_json_kv(out: &mut String, key: &str, value: &str, first: bool) {
    if !first {
        out.push(',');
    }
    out.push('"');
    escape_json_into(out, key);
    out.push_str("\":\"");
    escape_json_into(out, value);
    out.push('"');
}

fn escape_json_into(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

fn dismissals_dir() -> PathBuf {
    sessions_dir()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("agent-dismissals")
}

fn dismissal_path_from_dir(dir: &Path, agent_id: &str, session_id: &str) -> PathBuf {
    let key = format!("{agent_id}\0{session_id}");
    dir.join(format!("{}.json", crate::atomic::hash_hex(&key)))
}

fn dismissal_json(record: &SessionDismissal) -> String {
    serialize_event_json_with_meta(
        &AgentEventPayload {
            agent: record.agent.clone(),
            session_id: record.session_id.clone(),
            title: None,
            detail: None,
            cwd: None,
            state: ParsedEventState::Closed,
            hwnd: None,
            pid: None,
            deep_link: None,
            updated_at: Some(record.version.updated_at),
        },
        &EventMeta {
            instance: record.version.instance.clone(),
            seq: Some(record.version.seq),
        },
    )
}

/// Persist a user dismissal outside the producer-watched session directory.
/// Atomic replacement prevents half-written markers and the producer can keep
/// updating its own JSON without a delete/rename race.
pub fn persist_session_dismissal(record: &SessionDismissal) -> std::io::Result<()> {
    let dir = dismissals_dir();
    let path = dismissal_path_from_dir(&dir, &record.agent, &record.session_id);
    crate::atomic::write(&path, dismissal_json(record).as_bytes())
}

fn load_session_dismissals_from_dir(dir: &Path) -> std::io::Result<Vec<SessionDismissal>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        let metadata = entry.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_PAYLOAD_BYTES as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid dismissal marker: {}", path.display()),
            ));
        }
        let text = fs::read_to_string(&path)?;
        let invalid = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid dismissal marker: {}", path.display()),
            )
        };
        let Some((payload, meta)) = parse_agent_event_json_with_meta(&text) else {
            return Err(invalid());
        };
        if payload.state != ParsedEventState::Closed {
            return Err(invalid());
        }
        let Some(agent) = normalize_agent_id(&payload.agent) else {
            return Err(invalid());
        };
        let Some(session_id) = sanitize_id(&payload.session_id, MAX_ID_CHARS) else {
            return Err(invalid());
        };
        let Some(updated_at) = payload.updated_at else {
            return Err(invalid());
        };
        let Some(seq) = meta.seq else {
            return Err(invalid());
        };
        let instance = match meta.instance {
            Some(value) => Some(sanitize_instance(&value).ok_or_else(invalid)?),
            None => None,
        };
        if dismissal_path_from_dir(dir, &agent, &session_id) != path {
            return Err(invalid());
        }
        records.push(SessionDismissal {
            agent,
            session_id,
            version: SessionVersion {
                instance,
                seq,
                updated_at,
            },
        });
    }
    Ok(records)
}

pub fn load_session_dismissals() -> std::io::Result<Vec<SessionDismissal>> {
    load_session_dismissals_from_dir(&dismissals_dir())
}

#[cfg(test)]
pub fn parse_agent_event_json(input: &str) -> Option<AgentEventPayload> {
    parse_agent_event_json_with_meta(input).map(|(payload, _)| payload)
}

pub fn parse_agent_event_json_with_meta(input: &str) -> Option<(AgentEventPayload, EventMeta)> {
    if input.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    let trimmed = input.trim().trim_start_matches('\u{FEFF}');
    let mut parser = JsonCursor::new(trimmed);
    parser.expect_char('{')?;
    parser.skip_ws();

    let mut agent: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut title: Option<String> = None;
    let mut detail: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut state: Option<ParsedEventState> = None;
    let mut hwnd: Option<isize> = None;
    let mut pid: Option<u32> = None;
    let mut deep_link: Option<String> = None;
    let mut updated_at: Option<u64> = None;
    let mut instance: Option<String> = None;
    let mut seq: Option<u64> = None;

    if parser.peek_char() != Some('}') {
        loop {
            parser.skip_ws();
            let key = parser.parse_string()?;
            parser.skip_ws();
            parser.expect_char(':')?;
            parser.skip_ws();
            match key.as_str() {
                "agent" => agent = parser.parse_opt_string()?,
                "session_id" | "session" => session_id = parser.parse_opt_string()?,
                "title" => title = parser.parse_opt_string()?,
                "detail" => detail = parser.parse_opt_string()?,
                "cwd" => cwd = parser.parse_opt_string()?,
                "state" => {
                    let s = parser.parse_opt_string()?;
                    state = s.as_deref().and_then(ParsedEventState::parse);
                }
                "hwnd" => hwnd = parser.parse_opt_isize()?,
                "pid" => pid = parser.parse_opt_u32()?,
                "deep_link" => deep_link = parser.parse_opt_string()?,
                "updated_at" => updated_at = parser.parse_opt_u64()?,
                "instance" => instance = parser.parse_opt_string()?,
                "seq" => seq = parser.parse_opt_u64()?,
                _ => parser.skip_value(1)?,
            }
            parser.skip_ws();
            match parser.peek_char() {
                Some(',') => {
                    parser.advance();
                }
                Some('}') => break,
                _ => return None,
            }
        }
    }
    parser.expect_char('}')?;
    parser.skip_ws();
    if !parser.is_eof() {
        return None;
    }

    Some((
        AgentEventPayload {
            agent: agent?,
            session_id: session_id?,
            title,
            detail,
            cwd,
            state: state?,
            hwnd,
            pid,
            deep_link,
            updated_at,
        },
        EventMeta { instance, seq },
    ))
}

struct JsonCursor<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> JsonCursor<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek_char(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek_char(), Some(c) if c.is_ascii_whitespace()) {
            self.advance();
        }
    }

    fn expect_char(&mut self, expected: char) -> Option<()> {
        if self.advance() == Some(expected) {
            Some(())
        } else {
            None
        }
    }

    fn consume_literal(&mut self, lit: &str) -> bool {
        if self.rest().starts_with(lit) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        self.expect_char('"')?;
        let mut out = String::new();
        while let Some(ch) = self.advance() {
            match ch {
                '"' => return Some(out),
                '\\' => {
                    let esc = self.advance()?;
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{0008}'),
                        'f' => out.push('\u{000C}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let code = self.parse_hex4()?;
                            if (0xD800..=0xDBFF).contains(&code) {
                                if self.consume_literal("\\u") {
                                    let low = self.parse_hex4()?;
                                    if (0xDC00..=0xDFFF).contains(&low) {
                                        let combined = 0x10000
                                            + (((code as u32 - 0xD800) << 10)
                                                | (low as u32 - 0xDC00));
                                        if let Some(ch) = char::from_u32(combined) {
                                            out.push(ch);
                                            continue;
                                        }
                                    }
                                }
                                return None;
                            }
                            out.push(char::from_u32(code as u32)?);
                        }
                        _ => return None,
                    }
                }
                c if (c as u32) < 0x20 => return None,
                c => out.push(c),
            }
        }
        None
    }

    fn parse_hex4(&mut self) -> Option<u16> {
        let mut v = 0u16;
        for _ in 0..4 {
            let d = self.advance()?.to_digit(16)? as u16;
            v = (v << 4) | d;
        }
        Some(v)
    }

    fn parse_opt_string(&mut self) -> Option<Option<String>> {
        if self.consume_literal("null") {
            return Some(None);
        }
        self.parse_string().map(Some)
    }

    fn parse_int_token(&mut self) -> Option<&'a str> {
        let start = self.pos;
        if self.peek_char() == Some('-') {
            self.advance();
        }
        let digits_start = self.pos;
        while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
            self.advance();
        }
        if self.pos == digits_start {
            return None;
        }
        Some(&self.src[start..self.pos])
    }

    fn parse_opt_isize(&mut self) -> Option<Option<isize>> {
        if self.consume_literal("null") {
            return Some(None);
        }
        if self.peek_char() == Some('"') {
            let s = self.parse_string()?;
            return Some(s.trim().parse::<isize>().ok());
        }
        self.parse_int_token()
            .and_then(|s| s.parse::<isize>().ok())
            .map(Some)
    }

    fn parse_opt_u32(&mut self) -> Option<Option<u32>> {
        if self.consume_literal("null") {
            return Some(None);
        }
        if self.peek_char() == Some('"') {
            let s = self.parse_string()?;
            return Some(s.trim().parse::<u32>().ok());
        }
        self.parse_int_token()
            .and_then(|s| s.parse::<u32>().ok())
            .map(Some)
    }

    fn parse_opt_u64(&mut self) -> Option<Option<u64>> {
        if self.consume_literal("null") {
            return Some(None);
        }
        self.parse_int_token()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Some)
    }

    fn skip_value(&mut self, depth: usize) -> Option<()> {
        if depth >= MAX_JSON_DEPTH {
            return None;
        }
        self.skip_ws();
        match self.peek_char()? {
            '"' => {
                let _ = self.parse_string()?;
                Some(())
            }
            '{' => {
                self.advance();
                self.skip_ws();
                if self.peek_char() == Some('}') {
                    self.advance();
                    return Some(());
                }
                loop {
                    self.skip_ws();
                    let _ = self.parse_string()?;
                    self.skip_ws();
                    self.expect_char(':')?;
                    self.skip_value(depth + 1)?;
                    self.skip_ws();
                    match self.advance()? {
                        ',' => continue,
                        '}' => return Some(()),
                        _ => return None,
                    }
                }
            }
            '[' => {
                self.advance();
                self.skip_ws();
                if self.peek_char() == Some(']') {
                    self.advance();
                    return Some(());
                }
                loop {
                    self.skip_value(depth + 1)?;
                    self.skip_ws();
                    match self.advance()? {
                        ',' => continue,
                        ']' => return Some(()),
                        _ => return None,
                    }
                }
            }
            't' if self.consume_literal("true") => Some(()),
            'f' if self.consume_literal("false") => Some(()),
            'n' if self.consume_literal("null") => Some(()),
            '-' | '0'..='9' => {
                if self.peek_char() == Some('-') {
                    self.advance();
                }
                let digits_start = self.pos;
                while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                    self.advance();
                }
                if self.pos == digits_start {
                    return None;
                }
                if self.peek_char() == Some('.') {
                    self.advance();
                    let frac_start = self.pos;
                    while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                        self.advance();
                    }
                    if self.pos == frac_start {
                        return None;
                    }
                }
                if matches!(self.peek_char(), Some('e' | 'E')) {
                    self.advance();
                    if matches!(self.peek_char(), Some('+' | '-')) {
                        self.advance();
                    }
                    let exp_start = self.pos;
                    while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                        self.advance();
                    }
                    if self.pos == exp_start {
                        return None;
                    }
                }
                Some(())
            }
            _ => None,
        }
    }
}

// ---------- CLI `--agent-event` & `WM_COPYDATA` IPC ----------

pub fn parse_cli_event_args<I>(args: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter().peekable();
    let first = iter.next()?;
    if first != AGENT_EVENT_FLAG {
        return None;
    }
    let mut raw_json: Option<String> = None;
    let mut agent: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut state: Option<ParsedEventState> = None;
    let mut title: Option<String> = None;
    let mut detail: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut hwnd: Option<isize> = None;
    let mut pid: Option<u32> = None;
    let mut deep_link: Option<String> = None;
    let mut updated_at: Option<u64> = None;
    let mut instance: Option<String> = None;
    let mut seq: Option<u64> = None;

    while let Some(flag) = iter.next() {
        let value = iter.next()?;
        match flag.as_str() {
            "--json" => raw_json = Some(value),
            "--agent" => agent = Some(value),
            "--session" | "--session-id" => session_id = Some(value),
            "--state" => state = ParsedEventState::parse(&value),
            "--title" => title = Some(value),
            "--detail" => detail = Some(value),
            "--cwd" => cwd = Some(value),
            "--hwnd" => hwnd = value.trim().parse::<isize>().ok(),
            "--pid" => pid = value.trim().parse::<u32>().ok(),
            "--deep-link" => deep_link = Some(value),
            "--updated-at" => updated_at = value.trim().parse::<u64>().ok(),
            "--instance" => instance = Some(value),
            "--seq" => seq = value.trim().parse::<u64>().ok(),
            _ => {}
        }
    }

    if let Some(json) = raw_json {
        return parse_agent_event_json_with_meta(&json)
            .map(|(parsed, meta)| serialize_event_json_with_meta(&parsed, &meta));
    }

    let payload = AgentEventPayload {
        agent: agent?,
        session_id: session_id?,
        title,
        detail,
        cwd,
        state: state?,
        hwnd,
        pid,
        deep_link,
        updated_at: Some(updated_at.unwrap_or_else(now_unix_secs)),
    };
    Some(serialize_event_json_with_meta(
        &payload,
        &EventMeta { instance, seq },
    ))
}

/// Handle `FeatherDock.exe --agent-event ...`:
/// - `None`: normal Dock launch
/// - `Some(0)`: sent & accepted by running Dock (`WM_COPYDATA` returned `1`)
/// - `Some(2)`: invalid arguments or oversized payload
/// - `Some(3)`: no running `FeatherDockWindow` instance found
/// - `Some(4)`: `SendMessageTimeoutW` failed, timed out, or payload was rejected
pub unsafe fn handle_cli_agent_event() -> Option<i32> {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().map(String::as_str) != Some(AGENT_EVENT_FLAG) {
        return None;
    }
    let Some(json) = parse_cli_event_args(args) else {
        eprintln!("FeatherDock: invalid --agent-event arguments");
        return Some(2);
    };
    if json.len() > MAX_PAYLOAD_BYTES {
        eprintln!("FeatherDock: --agent-event payload exceeds 64 KB limit");
        return Some(2);
    }
    let hwnd =
        FindWindowW(windows::core::w!("FeatherDockWindow"), PCWSTR::null()).unwrap_or_default();
    if hwnd.is_invalid() {
        eprintln!("FeatherDock: no running FeatherDock instance found");
        return Some(3);
    }
    let bytes = json.as_bytes();
    let cds = COPYDATASTRUCT {
        dwData: AGENT_EVENT_COPYDATA_MAGIC,
        cbData: bytes.len() as u32,
        lpData: bytes.as_ptr() as *mut c_void,
    };
    let mut delivered = 0usize;
    let sent = SendMessageTimeoutW(
        hwnd,
        WM_COPYDATA,
        WPARAM(0),
        LPARAM(&cds as *const COPYDATASTRUCT as isize),
        SMTO_ABORTIFHUNG,
        IPC_TIMEOUT_MS,
        Some(&mut delivered),
    );
    if sent.0 == 0 || delivered == 0 {
        eprintln!("FeatherDock: failed to deliver --agent-event to running Dock");
        return Some(4);
    }
    Some(0)
}

#[allow(dead_code)]
pub unsafe fn decode_copydata(lparam: LPARAM) -> Option<AgentEventPayload> {
    decode_copydata_with_meta(lparam).map(|(p, _)| p)
}

pub unsafe fn is_agent_copydata(lparam: LPARAM) -> bool {
    if lparam.0 == 0 {
        return false;
    }
    let cds = &*(lparam.0 as *const COPYDATASTRUCT);
    cds.dwData == AGENT_EVENT_COPYDATA_MAGIC
}

pub unsafe fn decode_copydata_with_meta(lparam: LPARAM) -> Option<(AgentEventPayload, EventMeta)> {
    if lparam.0 == 0 {
        return None;
    }
    let cds = &*(lparam.0 as *const COPYDATASTRUCT);
    if cds.dwData != AGENT_EVENT_COPYDATA_MAGIC
        || cds.cbData == 0
        || cds.cbData as usize > MAX_PAYLOAD_BYTES
        || cds.lpData.is_null()
    {
        return None;
    }
    let bytes = std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize);
    let text = std::str::from_utf8(bytes).ok()?;
    parse_agent_event_json_with_meta(text)
}

// ---------- Kernel-event directory watcher (`ReadDirectoryChangesW`) ----------

pub struct SessionDirWatcher {
    stop_event: HANDLE,
    thread: Option<JoinHandle<()>>,
    latest: Arc<Mutex<Vec<(AgentSession, EventMeta)>>>,
}

impl SessionDirWatcher {
    pub unsafe fn start(dock_hwnd: HWND, initial: SessionSnapshot) -> Self {
        let dir = sessions_dir();
        let _ = fs::create_dir_all(&dir);
        let (init_cache, init_sessions) = initial;
        let latest = Arc::new(Mutex::new(init_sessions));
        let stop_event = CreateEventW(None, true, false, None).unwrap_or_default();
        if stop_event.is_invalid() {
            return Self {
                stop_event: HANDLE::default(),
                thread: None,
                latest,
            };
        }
        let dock_raw = dock_hwnd.0 as isize;
        let stop_raw = stop_event.0 as isize;
        let latest_ref = Arc::clone(&latest);
        let dir_clone = dir.clone();
        let thread = thread::spawn(move || {
            watch_dir_loop(
                HWND(dock_raw as *mut c_void),
                HANDLE(stop_raw as *mut c_void),
                &dir_clone,
                &latest_ref,
                init_cache,
            );
        });
        Self {
            stop_event,
            thread: Some(thread),
            latest,
        }
    }

    #[allow(dead_code)]
    pub fn take_snapshot(&self) -> Vec<AgentSession> {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(s, _)| s.clone())
            .collect()
    }

    pub fn take_snapshot_with_meta(&self) -> Vec<(AgentSession, EventMeta)> {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for SessionDirWatcher {
    fn drop(&mut self) {
        unsafe {
            if !self.stop_event.is_invalid() {
                let _ = SetEvent(self.stop_event);
            }
            if let Some(handle) = self.thread.take() {
                let _ = handle.join();
            }
            if !self.stop_event.is_invalid() {
                let _ = CloseHandle(self.stop_event);
            }
        }
    }
}

fn publish_loaded_sessions(
    loaded: std::io::Result<(CachedFileMap, Vec<(AgentSession, EventMeta)>)>,
    cached_files: &mut CachedFileMap,
    latest: &Arc<Mutex<Vec<(AgentSession, EventMeta)>>>,
    mut post_ready: impl FnMut(),
) -> bool {
    let Ok((next_cache, snapshot)) = loaded else {
        return false;
    };
    *cached_files = next_cache;
    *latest.lock().unwrap_or_else(|e| e.into_inner()) = snapshot;
    post_ready();
    true
}

unsafe fn watch_dir_loop(
    dock_hwnd: HWND,
    stop_event: HANDLE,
    dir: &Path,
    latest: &Arc<Mutex<Vec<(AgentSession, EventMeta)>>>,
    cached_files: CachedFileMap,
) {
    watch_dir_loop_with_loader(dock_hwnd, stop_event, dir, latest, cached_files, || {
        load_sessions_preserving(dir)
    });
}

unsafe fn reload_watcher_sessions<F>(
    dock_hwnd: HWND,
    stop_event: HANDLE,
    cached_files: &mut CachedFileMap,
    latest: &Arc<Mutex<Vec<(AgentSession, EventMeta)>>>,
    load: &mut F,
) -> bool
where
    F: FnMut() -> std::io::Result<SessionSnapshot>,
{
    let mut post_ready = || {
        if !dock_hwnd.is_invalid() {
            let _ = PostMessageW(dock_hwnd, WM_AGENT_SESSIONS_READY, WPARAM(0), LPARAM(0));
        }
    };
    if publish_loaded_sessions(load(), cached_files, latest, &mut post_ready) {
        return true;
    }
    // A consumed notification gets exactly one delayed retry. Stop remains interruptible;
    // persistent failures return to the infinite kernel-directory wait, never polling.
    if WaitForSingleObject(stop_event, WATCH_LOAD_RETRY_MS) == WAIT_OBJECT_0 {
        return false;
    }
    publish_loaded_sessions(load(), cached_files, latest, post_ready)
}

unsafe fn arm_directory_watch(
    dir_handle: HANDLE,
    io_event: HANDLE,
    buffer: &mut [u8],
    overlapped: &mut OVERLAPPED,
) -> bool {
    let _ = ResetEvent(io_event);
    *overlapped = OVERLAPPED {
        hEvent: io_event,
        ..Default::default()
    };
    ReadDirectoryChangesW(
        dir_handle,
        buffer.as_mut_ptr() as *mut c_void,
        buffer.len() as u32,
        false,
        FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE,
        None,
        Some(overlapped),
        None,
    )
    .is_ok()
}

unsafe fn cancel_armed_directory_watch(dir_handle: HANDLE, overlapped: &mut OVERLAPPED) {
    let _ = CancelIoEx(dir_handle, Some(overlapped));
    // Keep the overlapped request and its stack buffer alive until cancellation has
    // actually completed; CancelIoEx alone does not wait for the async operation.
    let mut bytes = 0u32;
    let _ = GetOverlappedResult(dir_handle, overlapped, &mut bytes, true);
}

unsafe fn watch_dir_loop_with_loader<F>(
    dock_hwnd: HWND,
    stop_event: HANDLE,
    dir: &Path,
    latest: &Arc<Mutex<Vec<(AgentSession, EventMeta)>>>,
    mut cached_files: CachedFileMap,
    mut load: F,
) where
    F: FnMut() -> std::io::Result<SessionSnapshot>,
{
    let wide_dir: Vec<u16> = dir
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let Ok(dir_handle) = CreateFileW(
        PCWSTR(wide_dir.as_ptr()),
        FILE_LIST_DIRECTORY.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        None,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
        None,
    ) else {
        return;
    };
    let Ok(io_event) = CreateEventW(None, true, false, None) else {
        let _ = CloseHandle(dir_handle);
        return;
    };

    let mut buffer = [0u8; 4096];
    let mut overlapped = OVERLAPPED::default();
    if !arm_directory_watch(dir_handle, io_event, &mut buffer, &mut overlapped) {
        let _ = CloseHandle(io_event);
        let _ = CloseHandle(dir_handle);
        return;
    }

    // The first load happens only after the kernel watch is armed. Updates before arm
    // are captured by this load; updates during it already have a pending watch.
    reload_watcher_sessions(dock_hwnd, stop_event, &mut cached_files, latest, &mut load);

    loop {
        let wait = WaitForMultipleObjects(&[stop_event, io_event], false, INFINITE);
        if wait.0 != WAIT_OBJECT_0.0 + 1 {
            cancel_armed_directory_watch(dir_handle, &mut overlapped);
            break;
        }

        let mut bytes = 0u32;
        if GetOverlappedResult(dir_handle, &overlapped, &mut bytes, false).is_err() {
            break;
        }
        // Close the steady-state gap before any coalescing, disk I/O, or retry wait.
        // Any update during those operations completes this newly armed request.
        if !arm_directory_watch(dir_handle, io_event, &mut buffer, &mut overlapped) {
            break;
        }

        // Briefly coalesce bursts of atomic temp-file writes/renames into one reload.
        if WaitForSingleObject(stop_event, 60) == WAIT_OBJECT_0 {
            cancel_armed_directory_watch(dir_handle, &mut overlapped);
            break;
        }
        reload_watcher_sessions(dock_hwnd, stop_event, &mut cached_files, latest, &mut load);
        // A failed enumeration keeps the last complete cache/snapshot, sends no READY,
        // and receives one delayed retry. Persistent failures wait for the next directory
        // event; only a successful empty enumeration confirms deletion.
    }

    let _ = CloseHandle(io_event);
    let _ = CloseHandle(dir_handle);
}

// ---------- Kernel-event PID exit monitor (`RegisterWaitForSingleObject`) ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PidWatchResult {
    Watching {
        proc_start: Option<u64>,
        generation: usize,
    },
    Exited,
    AccessDenied,
}

fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

unsafe fn query_process_start(proc_handle: HANDLE) -> Option<u64> {
    let mut create = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    GetProcessTimes(proc_handle, &mut create, &mut exit, &mut kernel, &mut user)
        .ok()
        .map(|()| filetime_to_u64(create))
}

unsafe fn process_has_exited(proc_handle: HANDLE) -> bool {
    let mut exit_code = 0u32;
    if GetExitCodeProcess(proc_handle, &mut exit_code).is_ok() && exit_code != STILL_ACTIVE.0 as u32
    {
        return true;
    }
    WaitForSingleObject(proc_handle, 0) == WAIT_OBJECT_0
}

pub unsafe fn process_creation_time_for_pid(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let start = if process_has_exited(handle) {
        None
    } else {
        query_process_start(handle)
    };
    let _ = CloseHandle(handle);
    start
}

struct PidWaitEntry {
    proc_handle: HANDLE,
    wait_handle: HANDLE,
    ctx: *mut PidWaitContext,
    proc_start: Option<u64>,
    generation: usize,
}

struct PidWaitContext {
    dock_raw: isize,
    pid: u32,
    generation: usize,
}

#[derive(Default)]
pub struct PidMonitor {
    entries: HashMap<u32, PidWaitEntry>,
    retired: HashMap<(u32, usize), Option<u64>>,
    retired_order: VecDeque<(u32, usize)>,
    next_generation: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PidCompletion {
    Matched { proc_start: Option<u64> },
    Retired { proc_start: Option<u64> },
    Ignored,
}

impl PidMonitor {
    pub unsafe fn ensure_watching(&mut self, dock_hwnd: HWND, pid: u32) -> PidWatchResult {
        if pid == 0 || dock_hwnd.is_invalid() {
            return PidWatchResult::AccessDenied;
        }
        if let Some(existing) = self.entries.get(&pid) {
            if !process_has_exited(existing.proc_handle) {
                return PidWatchResult::Watching {
                    proc_start: existing.proc_start,
                    generation: existing.generation,
                };
            }
            if let Some(old) = self.entries.remove(&pid) {
                self.retire_entry(pid, old);
            }
        }
        let proc_handle = match OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        ) {
            Ok(h) => h,
            Err(err) => {
                if err.code() == HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) {
                    return PidWatchResult::Exited;
                }
                if err.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
                    if let Ok(q) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                        let exited = process_has_exited(q);
                        let _ = CloseHandle(q);
                        return if exited {
                            PidWatchResult::Exited
                        } else {
                            PidWatchResult::AccessDenied
                        };
                    }
                    return PidWatchResult::AccessDenied;
                }
                match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                    Ok(h) => h,
                    Err(e2) if e2.code() == HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => {
                        return PidWatchResult::Exited;
                    }
                    Err(_) => return PidWatchResult::AccessDenied,
                }
            }
        };

        if process_has_exited(proc_handle) {
            let _ = CloseHandle(proc_handle);
            return PidWatchResult::Exited;
        }
        let proc_start = query_process_start(proc_handle);
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let ctx = Box::into_raw(Box::new(PidWaitContext {
            dock_raw: dock_hwnd.0 as isize,
            pid,
            generation,
        }));
        let mut wait_handle = HANDLE::default();
        if RegisterWaitForSingleObject(
            &mut wait_handle,
            proc_handle,
            Some(pid_exit_callback),
            Some(ctx as *const c_void),
            INFINITE,
            WT_EXECUTEONLYONCE,
        )
        .is_err()
        {
            drop(Box::from_raw(ctx));
            let _ = CloseHandle(proc_handle);
            return PidWatchResult::AccessDenied;
        }
        self.entries.insert(
            pid,
            PidWaitEntry {
                proc_handle,
                wait_handle,
                ctx,
                proc_start,
                generation,
            },
        );
        PidWatchResult::Watching {
            proc_start,
            generation,
        }
    }

    unsafe fn retire_entry(&mut self, pid: u32, entry: PidWaitEntry) {
        let key = (pid, entry.generation);
        self.retired.insert(key, entry.proc_start);
        self.retired_order.push_back(key);
        while self.retired_order.len() > MAX_RETIRED_PID_GENERATIONS {
            if let Some(oldest) = self.retired_order.pop_front() {
                self.retired.remove(&oldest);
            }
        }
        cleanup_pid_entry(entry);
    }

    unsafe fn complete(&mut self, pid: u32, generation: usize) -> PidCompletion {
        if self
            .entries
            .get(&pid)
            .is_some_and(|entry| entry.generation == generation)
        {
            let entry = self.entries.remove(&pid).expect("entry was just present");
            let proc_start = entry.proc_start;
            cleanup_pid_entry(entry);
            return PidCompletion::Matched { proc_start };
        }
        if let Some(proc_start) = self.retired.remove(&(pid, generation)) {
            return PidCompletion::Retired { proc_start };
        }
        PidCompletion::Ignored
    }

    /// A current-generation exit unregisters that watch and prunes its process identity.
    /// A queued retired-generation exit may prune only its old `proc_start` or watch
    /// generation; it never unregisters or wildcard-prunes a reused PID's current state.
    pub unsafe fn complete_and_prune(
        &mut self,
        registry: &mut AgentRegistry,
        pid: u32,
        generation: usize,
    ) -> bool {
        match self.complete(pid, generation) {
            PidCompletion::Matched {
                proc_start: Some(proc_start),
            }
            | PidCompletion::Retired {
                proc_start: Some(proc_start),
            } => registry.prune_pid_with_start(pid, Some(proc_start)),
            PidCompletion::Matched { proc_start: None }
            | PidCompletion::Retired { proc_start: None } => {
                registry.prune_pid_with_generation(pid, generation)
            }
            PidCompletion::Ignored => false,
        }
    }

    /// Release a watch only after the registry proves that no remaining session
    /// uses this PID. `UnregisterWaitEx` drains any in-flight callback before the
    /// context is freed, matching the existing monitor cleanup path.
    pub unsafe fn unwatch(&mut self, pid: u32) -> bool {
        let Some(entry) = self.entries.remove(&pid) else {
            return false;
        };
        cleanup_pid_entry(entry);
        true
    }

    pub unsafe fn clear(&mut self) {
        for (_, entry) in self.entries.drain() {
            cleanup_pid_entry(entry);
        }
        self.retired.clear();
        self.retired_order.clear();
    }
}

impl Drop for PidMonitor {
    fn drop(&mut self) {
        unsafe { self.clear() };
    }
}

unsafe fn cleanup_pid_entry(entry: PidWaitEntry) {
    let _ = UnregisterWaitEx(entry.wait_handle, INVALID_HANDLE_VALUE);
    let _ = CloseHandle(entry.proc_handle);
    if !entry.ctx.is_null() {
        drop(Box::from_raw(entry.ctx));
    }
}

unsafe extern "system" fn pid_exit_callback(ctx: *mut c_void, _timed_out: BOOLEAN) {
    if ctx.is_null() {
        return;
    }
    let ctx = &*(ctx as *const PidWaitContext);
    let hwnd = HWND(ctx.dock_raw as *mut c_void);
    if !hwnd.is_invalid() {
        let _ = PostMessageW(
            hwnd,
            WM_AGENT_PROCESS_EXITED,
            WPARAM(ctx.pid as usize),
            LPARAM(ctx.generation as isize),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use windows::core::w;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, PeekMessageW, HWND_MESSAGE, MSG, PM_REMOVE, WS_OVERLAPPED,
    };

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("featherdock-agent-{nonce}-{name}"))
    }

    struct TestWindow(HWND);

    impl TestWindow {
        fn new() -> Self {
            let instance = unsafe { GetModuleHandleW(None) }.unwrap();
            let hwnd = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("Static"),
                    w!("FeatherDock watcher test"),
                    WS_OVERLAPPED,
                    0,
                    0,
                    1,
                    1,
                    HWND_MESSAGE,
                    None,
                    instance,
                    None,
                )
            }
            .unwrap();
            Self(hwnd)
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    struct TestWatcher {
        stop_event: HANDLE,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for TestWatcher {
        fn drop(&mut self) {
            unsafe {
                let _ = SetEvent(self.stop_event);
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
                let _ = CloseHandle(self.stop_event);
            }
        }
    }

    fn wait_for_test_message(hwnd: HWND, message_id: u32) -> Option<MSG> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut message = MSG::default();
            if unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                if message.hwnd == hwnd && message.message == message_id {
                    return Some(message);
                }
            } else if Instant::now() >= deadline {
                return None;
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn p0_event(session_id: &str, state: ParsedEventState, updated_at: u64) -> AgentEventPayload {
        AgentEventPayload {
            agent: "codex".into(),
            session_id: session_id.into(),
            title: Some(session_id.into()),
            detail: None,
            cwd: None,
            state,
            hwnd: None,
            pid: None,
            deep_link: None,
            updated_at: Some(updated_at),
        }
    }

    fn p0_meta(instance: &str, seq: u64) -> EventMeta {
        EventMeta {
            instance: Some(instance.into()),
            seq: Some(seq),
        }
    }

    fn p0_file_session(session_id: &str, state: AgentState, updated_at: u64) -> AgentSession {
        AgentSession {
            agent: "codex".into(),
            session_id: session_id.into(),
            title: session_id.into(),
            detail: String::new(),
            cwd: None,
            state,
            hwnd: None,
            pid: Some(4242),
            deep_link: None,
            updated_at,
            source: SessionSource::File,
        }
    }

    fn pid_test_entry(generation: usize, proc_start: Option<u64>) -> PidWaitEntry {
        // Register a real wait on an owned unsignalled event so UnregisterWaitEx and
        // handle cleanup follow the same valid Win32 path as production entries.
        unsafe {
            let proc_handle = CreateEventW(None, true, false, None).unwrap();
            let mut wait_handle = HANDLE::default();
            RegisterWaitForSingleObject(
                &mut wait_handle,
                proc_handle,
                Some(pid_exit_callback),
                None,
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
            .unwrap();
            PidWaitEntry {
                proc_handle,
                wait_handle,
                ctx: std::ptr::null_mut(),
                proc_start,
                generation,
            }
        }
    }

    #[test]
    fn parses_agent_event_json_with_windows_paths_and_unicode() {
        let json = r#"{
            "agent": "Codex",
            "session_id": "dock-refactor",
            "title": "Dock · 状态灯改造",
            "detail": "等待确认执行 \"cargo test\"",
            "cwd": "C:\\Work\\Dock",
            "state": "waiting",
            "hwnd": 131420,
            "pid": 24816,
            "deep_link": "codex://sessions/dock-refactor",
            "updated_at": 1794649310,
            "extra": {"nested": [1, 2, true, null]}
        }"#;
        let parsed = parse_agent_event_json(json).expect("valid payload");
        assert_eq!(parsed.agent, "Codex");
        assert_eq!(parsed.session_id, "dock-refactor");
        assert_eq!(parsed.title.as_deref(), Some("Dock · 状态灯改造"));
        assert_eq!(
            parsed.detail.as_deref(),
            Some("等待确认执行 \"cargo test\"")
        );
        assert_eq!(parsed.cwd.as_deref(), Some(r"C:\Work\Dock"));
        assert_eq!(parsed.state, ParsedEventState::State(AgentState::Waiting));
        assert_eq!(parsed.hwnd, Some(131420));
        assert_eq!(parsed.pid, Some(24816));
        assert_eq!(
            parsed.deep_link.as_deref(),
            Some("codex://sessions/dock-refactor")
        );
        assert_eq!(parsed.updated_at, Some(1794649310));
    }

    #[test]
    fn cli_args_serialize_and_roundtrip_through_json_parser() {
        let json = parse_cli_event_args(
            [
                "--agent-event",
                "--agent",
                "dsh",
                "--session",
                "s1",
                "--state",
                "running",
                "--cwd",
                r"C:\Work\Api",
                "--hwnd",
                "999",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("serialized json");
        let parsed = parse_agent_event_json(&json).expect("roundtrip");
        assert_eq!(parsed.agent, "dsh");
        assert_eq!(parsed.session_id, "s1");
        assert_eq!(parsed.state, ParsedEventState::State(AgentState::Running));
        assert_eq!(parsed.cwd.as_deref(), Some(r"C:\Work\Api"));
        assert_eq!(parsed.hwnd, Some(999));
    }

    #[test]
    fn deep_link_whitelist_rejects_shell_or_http_urls() {
        assert_eq!(
            validate_deep_link("codex://session/abc-1"),
            Some("codex://session/abc-1".to_string())
        );
        assert_eq!(
            validate_deep_link("antigravity://workspace/dock"),
            Some("antigravity://workspace/dock".to_string())
        );
        assert_eq!(
            validate_deep_link("dsh://tab/1"),
            Some("dsh://tab/1".to_string())
        );
        assert_eq!(
            validate_deep_link("vscode://file/C:/Work/Dock/src/main.rs:10:2"),
            Some("vscode://file/C:/Work/Dock/src/main.rs:10:2".to_string())
        );
        assert_eq!(
            validate_deep_link("cursor://file/C:/Work/Dock"),
            Some("cursor://file/C:/Work/Dock".to_string())
        );
        assert!(validate_deep_link("cmd://calc").is_none());
        assert!(validate_deep_link("https://example.com").is_none());
        assert!(validate_deep_link("file:///C:/Windows/System32/cmd.exe").is_none());
        assert!(validate_deep_link("codex://s?q=1;rm -rf").is_none());
        assert!(validate_deep_link("codex://").is_none());
    }

    #[test]
    fn built_in_matcher_distinguishes_agents_without_false_positives() {
        assert_eq!(
            infer_agent_from_exe_or_aumid(
                Some(r"C:\Users\u\AppData\Local\Programs\Antigravity\Antigravity.exe"),
                None,
            )
            .as_deref(),
            Some("antigravity")
        );
        assert_eq!(
            infer_agent_from_exe_or_aumid(
                Some(r"C:\Program Files\WindowsApps\OpenAI.Codex_26.0_x64__abc\app\Codex.exe"),
                Some("aumid:openai.codex_abc!app"),
            )
            .as_deref(),
            Some("codex")
        );
        assert_eq!(
            infer_agent_from_exe_or_aumid(Some(r"C:\Tools\dsh.exe"), None).as_deref(),
            Some("dsh")
        );
        assert!(infer_agent_from_exe_or_aumid(Some(r"C:\Games\bloodshed.exe"), None).is_none());
        assert!(infer_agent_from_exe_or_aumid(Some(r"C:\Tools\windshell.exe"), None).is_none());
    }

    #[test]
    fn history_window_hides_only_expired_idle_track_b_sessions() {
        let now = now_unix_secs();
        let old = now.saturating_sub(3600);
        let mut reg = AgentRegistry::default();
        reg.set_history_window(Some(1800));
        reg.file_sessions = vec![
            p0_file_session("stale-idle", AgentState::Idle, old),
            p0_file_session("recent-idle", AgentState::Idle, now - 60),
            p0_file_session("boundary-idle", AgentState::Idle, now - 1800),
            p0_file_session("future-idle", AgentState::Idle, now + 600),
            p0_file_session("running", AgentState::Running, old),
            p0_file_session("waiting", AgentState::Waiting, old),
            p0_file_session("error", AgentState::Error, old),
            p0_file_session("done", AgentState::Done, old),
        ];

        let summary = reg.summarize_agent("codex", "Codex", &[]);
        let visible: HashSet<&str> = summary
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect();
        assert!(!visible.contains("stale-idle"));
        for expected in [
            "recent-idle",
            "boundary-idle",
            "future-idle",
            "running",
            "waiting",
            "error",
            "done",
        ] {
            assert!(visible.contains(expected), "missing {expected}");
        }
        assert!(!summary
            .versions
            .contains_key(&("codex".into(), "stale-idle".into())));
        assert_eq!(summary.total_sessions, visible.len());
        assert_eq!(summary.aggregate_state, AgentState::Waiting);

        reg.set_history_window(None);
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 8);
        assert_eq!(AgentState::Waiting.label_en(), "question");
        assert_eq!(AgentState::Running.label_en(), "running");
        assert_eq!(AgentState::Idle.label_en(), "idle");
    }

    #[test]
    fn history_window_preserves_window_fallback_and_allows_reactivation() {
        let now = now_unix_secs();
        let mut reg = AgentRegistry::default();
        reg.set_history_window(Some(1800));
        reg.file_sessions = vec![p0_file_session("stale", AgentState::Idle, now - 7200)];
        let windows = [RunningWindowRef {
            hwnd: 42,
            title: "Codex window".into(),
        }];
        let summary = reg.summarize_agent("codex", "Codex", &windows);
        assert_eq!(summary.total_sessions, 1);
        assert_eq!(summary.sessions[0].source, SessionSource::Window);
        assert_eq!(summary.sessions[0].hwnd, Some(42));

        reg.apply_event(
            p0_event(
                "stale",
                ParsedEventState::State(AgentState::Running),
                now + 1,
            ),
            None,
            &[],
        );
        let reactivated = reg.summarize_agent("codex", "Codex", &[]);
        assert_eq!(reactivated.total_sessions, 1);
        assert_eq!(reactivated.sessions[0].session_id, "stale");
        assert_eq!(reactivated.aggregate_state, AgentState::Running);
    }

    #[test]
    fn priority_bubbles_waiting_over_error_running_done_idle_and_computes_badges() {
        let mut reg = AgentRegistry::default();
        let e1 = reg.apply_event(
            AgentEventPayload {
                agent: "codex".into(),
                session_id: "s1".into(),
                title: Some("A".into()),
                detail: Some("running".into()),
                cwd: None,
                state: ParsedEventState::State(AgentState::Running),
                hwnd: Some(10),
                pid: Some(100),
                deep_link: None,
                updated_at: Some(100),
            },
            None,
            &[],
        );
        assert_eq!(e1.bounce_agent, None);

        let e2 = reg.apply_event(
            AgentEventPayload {
                agent: "codex".into(),
                session_id: "s2".into(),
                title: Some("B".into()),
                detail: Some("confirm".into()),
                cwd: None,
                state: ParsedEventState::State(AgentState::Waiting),
                hwnd: Some(10),
                pid: Some(100),
                deep_link: None,
                updated_at: Some(101),
            },
            None,
            &[],
        );
        assert_eq!(e2.bounce_agent.as_deref(), Some("codex"));

        // Repeated waiting event for same session does not re-bounce.
        let e2_repeat = reg.apply_event(
            AgentEventPayload {
                agent: "codex".into(),
                session_id: "s2".into(),
                title: Some("B".into()),
                detail: Some("still confirm".into()),
                cwd: None,
                state: ParsedEventState::State(AgentState::Waiting),
                hwnd: Some(10),
                pid: Some(100),
                deep_link: None,
                updated_at: Some(102),
            },
            None,
            &[],
        );
        assert_eq!(e2_repeat.bounce_agent, None);

        let summary = reg.summarize_agent(
            "codex",
            "Codex",
            &[RunningWindowRef {
                hwnd: 10,
                title: "Codex".into(),
            }],
        );
        assert_eq!(summary.aggregate_state, AgentState::Waiting);
        assert_eq!(summary.badge_text.as_deref(), Some("1!"));
        assert_eq!(summary.sessions.len(), 2);
        assert_eq!(summary.sessions[0].session_id, "s2");
    }

    #[test]
    fn foreground_focus_clears_done_state_even_across_file_snapshots() {
        let dir = temp_dir("fg-clear");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("codex__s1.json"),
            r#"{"agent":"codex","session_id":"s1","state":"done","hwnd":555,"updated_at":200}"#,
        )
        .unwrap();
        let mut reg = AgentRegistry::default();
        let snap = load_sessions_from_dir(&dir);
        reg.replace_file_sessions(snap.clone(), None, &[]);
        assert_eq!(
            reg.summarize_agent("codex", "Codex", &[]).aggregate_state,
            AgentState::Done
        );

        assert!(reg.on_foreground_changed(Some(555), &[]));
        assert_eq!(
            reg.summarize_agent("codex", "Codex", &[]).aggregate_state,
            AgentState::Idle
        );

        // Unchanged timestamp on disk stays read/Idle until updated_at advances.
        reg.replace_file_sessions(snap, None, &[]);
        assert_eq!(
            reg.summarize_agent("codex", "Codex", &[]).aggregate_state,
            AgentState::Idle
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pid_and_dead_window_pruning_remove_stale_sessions() {
        let mut reg = AgentRegistry::default();
        reg.apply_event(
            AgentEventPayload {
                agent: "antigravity".into(),
                session_id: "s1".into(),
                title: Some("App".into()),
                detail: None,
                cwd: None,
                state: ParsedEventState::State(AgentState::Running),
                hwnd: Some(111),
                pid: Some(4242),
                deep_link: None,
                updated_at: Some(50),
            },
            None,
            &[],
        );
        assert!(reg.prune_dead_windows(|h| h != 111));
        assert!(reg.track_b_agent_ids().is_empty());

        reg.apply_event(
            AgentEventPayload {
                agent: "dsh".into(),
                session_id: "s2".into(),
                title: Some("Run".into()),
                detail: None,
                cwd: None,
                state: ParsedEventState::State(AgentState::Running),
                hwnd: None,
                pid: Some(9876),
                deep_link: None,
                updated_at: Some(51),
            },
            None,
            &[],
        );
        assert!(reg.prune_pid(9876));
        assert!(reg.track_b_agent_ids().is_empty());
    }

    #[test]
    fn json_parser_bounds_unknown_field_nesting() {
        let deep = format!(
            "{{\"agent\":\"codex\",\"session_id\":\"s\",\"state\":\"running\",\"extra\":{}0{}}}",
            "[".repeat(256),
            "]".repeat(256)
        );
        assert!(parse_agent_event_json(&deep).is_none());

        let probe = format!(
            "{{\"agent\":\"codex\",\"session_id\":\"s\",\"state\":\"running\",\"extra\":{}0{}}}",
            "[".repeat(20_000),
            "]".repeat(20_000)
        );
        assert!(parse_agent_event_json(&probe).is_none());
    }

    #[test]
    fn other_window_focus_preserves_unread_done_and_waiting_bounce() {
        let mut reg = AgentRegistry::default();
        let e1 = reg.apply_event(
            AgentEventPayload {
                agent: "codex".into(),
                session_id: "s1".into(),
                title: None,
                detail: None,
                cwd: None,
                state: ParsedEventState::State(AgentState::Done),
                hwnd: Some(222),
                pid: None,
                deep_link: None,
                updated_at: Some(100),
            },
            None,
            &[],
        );
        assert!(e1.changed);
        assert!(!reg.on_foreground_changed(Some(111), &["codex".into()]));
        assert_eq!(
            reg.summarize_agent("codex", "Codex", &[]).aggregate_state,
            AgentState::Done
        );

        let e2 = reg.apply_event(
            AgentEventPayload {
                agent: "codex".into(),
                session_id: "s2".into(),
                title: None,
                detail: None,
                cwd: None,
                state: ParsedEventState::State(AgentState::Waiting),
                hwnd: Some(222),
                pid: None,
                deep_link: None,
                updated_at: Some(101),
            },
            Some(111),
            &["codex".into()],
        );
        assert_eq!(e2.bounce_agent.as_deref(), Some("codex"));
    }

    #[test]
    fn dead_file_window_without_pid_stops_running_and_tombstone_blocks_reload() {
        let mut reg = AgentRegistry::default();
        let session = AgentSession {
            agent: "codex".into(),
            session_id: "file1".into(),
            title: "File session".into(),
            detail: String::new(),
            cwd: None,
            state: AgentState::Running,
            hwnd: Some(222),
            pid: None,
            deep_link: None,
            updated_at: 100,
            source: SessionSource::File,
        };
        reg.replace_file_sessions(vec![session.clone()], None, &[]);
        assert!(reg.prune_dead_windows(|_| false));
        assert_ne!(
            reg.summarize_agent("codex", "Codex", &[]).aggregate_state,
            AgentState::Running
        );
        // Unchanged disk file from before window close must not resurrect Running.
        reg.replace_file_sessions(vec![session], None, &[]);
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 0);
    }

    #[test]
    fn dead_process_is_not_resurrected_by_unchanged_file() {
        let mut reg = AgentRegistry::default();
        let session = AgentSession {
            agent: "codex".into(),
            session_id: "file1".into(),
            title: "File session".into(),
            detail: String::new(),
            cwd: None,
            state: AgentState::Running,
            hwnd: Some(222),
            pid: Some(4242),
            deep_link: None,
            updated_at: 100,
            source: SessionSource::File,
        };
        reg.replace_file_sessions(vec![session.clone()], None, &[]);
        assert!(reg.prune_pid(4242));
        reg.replace_file_sessions(vec![session], None, &[]);
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 0);
    }

    #[test]
    fn verified_background_pid_keeps_session_when_window_closes_until_pid_exits() {
        let mut reg = AgentRegistry::default();
        let session = AgentSession {
            agent: "codex".into(),
            session_id: "bg1".into(),
            title: "Background build".into(),
            detail: String::new(),
            cwd: None,
            state: AgentState::Running,
            hwnd: Some(222),
            pid: Some(4242),
            deep_link: None,
            updated_at: 100,
            source: SessionSource::File,
        };
        reg.replace_file_sessions(vec![session.clone()], None, &[]);
        reg.set_pid_proc_start(4242, Some(777_000));
        assert!(reg.prune_dead_windows(|_| false));
        let summary = reg.summarize_agent("codex", "Codex", &[]);
        assert_eq!(summary.aggregate_state, AgentState::Running);
        assert_eq!(summary.sessions[0].hwnd, None);
        // Stale snapshot still referencing the closed HWND must not re-attach it.
        reg.replace_file_sessions(vec![session.clone()], None, &[]);
        // Legacy snapshots have no instance proof, so production immediately rebinds
        // the returned PID through the existing watcher before handling its exit.
        reg.set_pid_proc_start(4242, Some(777_000));
        assert_eq!(
            reg.summarize_agent("codex", "Codex", &[]).sessions[0].hwnd,
            None
        );
        // Exiting the verified process removes and tombstones the session.
        assert!(reg.prune_pid_with_start(4242, Some(777_000)));
        reg.replace_file_sessions(vec![session], None, &[]);
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 0);
    }

    #[test]
    fn older_event_and_sequence_cannot_overwrite_newer_state_or_closed() {
        let mut reg = AgentRegistry::default();
        reg.apply_event(
            AgentEventPayload {
                agent: "codex".into(),
                session_id: "s1".into(),
                title: None,
                detail: None,
                cwd: None,
                state: ParsedEventState::State(AgentState::Done),
                hwnd: None,
                pid: None,
                deep_link: None,
                updated_at: Some(200),
            },
            None,
            &[],
        );
        let stale = reg.apply_event(
            AgentEventPayload {
                agent: "codex".into(),
                session_id: "s1".into(),
                title: None,
                detail: None,
                cwd: None,
                state: ParsedEventState::State(AgentState::Running),
                hwnd: None,
                pid: None,
                deep_link: None,
                updated_at: Some(100),
            },
            None,
            &[],
        );
        assert!(!stale.changed);
        assert_eq!(
            reg.summarize_agent("codex", "Codex", &[]).aggregate_state,
            AgentState::Done
        );

        // Same producer instance uses monotonic seq even within the same second.
        reg.apply_event_with_meta(
            AgentEventPayload {
                agent: "dsh".into(),
                session_id: "seq1".into(),
                title: None,
                detail: None,
                cwd: None,
                state: ParsedEventState::Closed,
                hwnd: None,
                pid: None,
                deep_link: None,
                updated_at: Some(300),
            },
            EventMeta {
                instance: Some("inst-a".into()),
                seq: Some(5),
            },
            None,
            &[],
        );
        let delayed = reg.apply_event_with_meta(
            AgentEventPayload {
                agent: "dsh".into(),
                session_id: "seq1".into(),
                title: None,
                detail: None,
                cwd: None,
                state: ParsedEventState::State(AgentState::Running),
                hwnd: None,
                pid: None,
                deep_link: None,
                updated_at: Some(300),
            },
            EventMeta {
                instance: Some("inst-a".into()),
                seq: Some(4),
            },
            None,
            &[],
        );
        assert!(!delayed.changed);
        assert_eq!(reg.summarize_agent("dsh", "dsh", &[]).total_sessions, 0);
    }

    #[test]
    fn file_tombstone_survives_confirmed_delete_and_same_version_reappearance() {
        let dir = temp_dir("tombstone-delete-reappear");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("codex__file1.json");
        let json = r#"{"agent":"codex","session_id":"file1","state":"running","pid":4242,"updated_at":100,"instance":"inst-a","seq":7}"#;
        fs::write(&path, json).unwrap();

        let (_, snapshot) = load_sessions_preserving(&dir).unwrap();
        let mut reg = AgentRegistry::default();
        reg.replace_file_sessions_with_meta(snapshot, None, &[]);
        assert!(reg.prune_pid(4242));
        assert!(reg
            .tombstones
            .contains_key(&("codex".into(), "file1".into())));

        fs::remove_file(&path).unwrap();
        let (_, confirmed_empty) = load_sessions_preserving(&dir).unwrap();
        assert!(confirmed_empty.is_empty());
        reg.replace_file_sessions_with_meta(confirmed_empty, None, &[]);
        let key = ("codex".to_string(), "file1".to_string());
        assert_eq!(
            reg.tombstones.get(&key).map(|tomb| tomb.kind),
            Some(TombstoneKind::DeadLifecycle)
        );

        // The exact same producer version reappears after a confirmed missing-file snapshot.
        fs::write(&path, json).unwrap();
        let (_, reappeared) = load_sessions_preserving(&dir).unwrap();
        assert_eq!(reappeared[0].1.seq, Some(7));
        reg.replace_file_sessions_with_meta(reappeared, None, &[]);
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 0);
        assert_eq!(reg.tombstones.get(&key).map(|tomb| tomb.stamp.seq), Some(7));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_read_failure_is_not_a_confirmed_empty_snapshot() {
        let dir = temp_dir("directory-read-failure");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("codex__file1.json"),
            r#"{"agent":"codex","session_id":"file1","state":"running","pid":4242,"updated_at":100,"instance":"inst-a","seq":7}"#,
        )
        .unwrap();
        let (cache, snapshot) = load_sessions_preserving(&dir).unwrap();
        assert_eq!(snapshot.len(), 1);

        fs::remove_dir_all(&dir).unwrap();
        assert!(load_sessions_preserving(&dir).is_err());
        assert_eq!(cache.len(), 1);

        // Only a successful enumeration may publish an empty list and clear live file rows.
        fs::create_dir_all(&dir).unwrap();
        let (_, confirmed_empty) = load_sessions_preserving(&dir).unwrap();
        assert!(confirmed_empty.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_closed_cannot_downgrade_tombstone_or_reopen_running() {
        let mut reg = AgentRegistry::default();
        reg.apply_event_with_meta(
            p0_event("seq-close", ParsedEventState::Closed, 1_000),
            p0_meta("inst-a", 300),
            None,
            &[],
        );
        let key = ("codex".to_string(), "seq-close".to_string());
        assert_eq!(
            reg.tombstones.get(&key).map(|tomb| tomb.stamp.seq),
            Some(300)
        );

        let stale_close = reg.apply_event_with_meta(
            p0_event("seq-close", ParsedEventState::Closed, 1_000),
            p0_meta("inst-a", 100),
            None,
            &[],
        );
        assert!(!stale_close.changed);
        assert_eq!(
            reg.tombstones.get(&key).map(|tomb| tomb.stamp.seq),
            Some(300)
        );

        let delayed_running = reg.apply_event_with_meta(
            p0_event(
                "seq-close",
                ParsedEventState::State(AgentState::Running),
                1_000,
            ),
            p0_meta("inst-a", 200),
            None,
            &[],
        );
        assert!(!delayed_running.changed);
        assert_eq!(
            reg.tombstones.get(&key).map(|tomb| tomb.stamp.seq),
            Some(300)
        );
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 0);
    }

    #[test]
    fn closed_arbitrates_against_file_and_clears_both_channels() {
        let mut reg = AgentRegistry::default();
        let file = p0_file_session("cross-close", AgentState::Running, 1_000);
        reg.replace_file_sessions_with_meta(
            vec![(file.clone(), p0_meta("inst-a", 300))],
            None,
            &[],
        );

        let stale_close = reg.apply_event_with_meta(
            p0_event("cross-close", ParsedEventState::Closed, 1_000),
            p0_meta("inst-a", 100),
            None,
            &[],
        );
        assert!(!stale_close.changed);
        let merged = reg.summarize_agent("codex", "Codex", &[]);
        assert_eq!(merged.aggregate_state, AgentState::Running);
        assert_eq!(merged.sessions[0].source, SessionSource::File);

        let new_close = reg.apply_event_with_meta(
            p0_event("cross-close", ParsedEventState::Closed, 1_000),
            p0_meta("inst-a", 400),
            None,
            &[],
        );
        assert!(new_close.changed);
        assert!(reg.event_sessions.is_empty());
        assert!(reg.file_sessions.is_empty());
        let key = ("codex".to_string(), "cross-close".to_string());
        assert!(!reg.event_versions.contains_key(&key));
        assert!(!reg.file_versions.contains_key(&key));
        assert_eq!(
            reg.tombstones.get(&key).map(|tomb| tomb.stamp.seq),
            Some(400)
        );

        reg.replace_file_sessions_with_meta(vec![(file, p0_meta("inst-a", 300))], None, &[]);
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 0);
    }

    #[test]
    fn file_and_event_channels_merge_with_their_own_version_stamps() {
        let mut reg = AgentRegistry::default();
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("shared-seq", AgentState::Running, 400),
                p0_meta("inst-a", 100),
            )],
            None,
            &[],
        );
        reg.apply_event_with_meta(
            p0_event(
                "shared-seq",
                ParsedEventState::State(AgentState::Waiting),
                300,
            ),
            p0_meta("inst-a", 200),
            None,
            &[],
        );
        let merged = reg.summarize_agent("codex", "Codex", &[]);
        assert_eq!(merged.aggregate_state, AgentState::Waiting);
        assert_eq!(merged.sessions[0].source, SessionSource::Event);
        let key = ("codex".to_string(), "shared-seq".to_string());
        assert_eq!(
            reg.file_versions.get(&key).map(|stamp| stamp.seq),
            Some(100)
        );
        assert_eq!(
            reg.event_versions.get(&key).map(|stamp| stamp.seq),
            Some(200)
        );

        let delayed_event = reg.apply_event_with_meta(
            p0_event(
                "shared-seq",
                ParsedEventState::State(AgentState::Running),
                250,
            ),
            p0_meta("inst-a", 150),
            None,
            &[],
        );
        assert!(!delayed_event.changed);
        assert_eq!(
            reg.summarize_agent("codex", "Codex", &[]).aggregate_state,
            AgentState::Waiting
        );
        assert_eq!(
            reg.event_versions.get(&key).map(|stamp| stamp.seq),
            Some(200)
        );

        // A later-arriving file with a wall-clock-newer but sequence-older version stays behind.
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("shared-seq", AgentState::Running, 500),
                p0_meta("inst-a", 100),
            )],
            None,
            &[],
        );
        assert_eq!(
            reg.summarize_agent("codex", "Codex", &[]).aggregate_state,
            AgentState::Waiting
        );

        // Different producer instances use updated_at, not an unrelated channel's seq.
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("new-instance", AgentState::Running, 400),
                p0_meta("inst-file", 900),
            )],
            None,
            &[],
        );
        reg.apply_event_with_meta(
            p0_event(
                "new-instance",
                ParsedEventState::State(AgentState::Done),
                500,
            ),
            p0_meta("inst-event", 1),
            None,
            &[],
        );
        let merged = reg.summarize_agent("codex", "Codex", &[]);
        let session = merged
            .sessions
            .iter()
            .find(|session| session.session_id == "new-instance")
            .unwrap();
        assert_eq!(session.state, AgentState::Done);
        assert_eq!(session.source, SessionSource::Event);

        reg.apply_event_with_meta(
            p0_event(
                "stale-file-side-effect",
                ParsedEventState::State(AgentState::Running),
                500,
            ),
            p0_meta("inst-a", 200),
            None,
            &[],
        );
        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("stale-file-side-effect", AgentState::Waiting, 400),
                p0_meta("inst-a", 100),
            )],
            None,
            &[],
        );
        assert!(bounces.is_empty());
        assert!(!reg.file_sessions.iter().any(|session| {
            session.agent == "codex" && session.session_id == "stale-file-side-effect"
        }));
        let merged = reg.summarize_agent("codex", "Codex", &[]);
        let session = merged
            .sessions
            .iter()
            .find(|session| session.session_id == "stale-file-side-effect")
            .unwrap();
        assert_eq!(session.state, AgentState::Running);
        assert_eq!(session.source, SessionSource::Event);

        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("stale-file-side-effect", AgentState::Waiting, 400),
                p0_meta("inst-a", 300),
            )],
            None,
            &[],
        );
        assert_eq!(bounces, vec!["codex".to_string()]);
        let merged = reg.summarize_agent("codex", "Codex", &[]);
        let session = merged
            .sessions
            .iter()
            .find(|session| session.session_id == "stale-file-side-effect")
            .unwrap();
        assert_eq!(session.state, AgentState::Waiting);
        assert_eq!(session.source, SessionSource::File);
    }

    #[test]
    fn queued_old_exit_after_pid_reuse_preserves_new_watch_and_running_session() {
        let mut reg = AgentRegistry::default();
        let key = ("codex".to_string(), "pid-reuse".to_string());
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("pid-reuse", AgentState::Running, 100),
                p0_meta("inst-old", 1),
            )],
            None,
            &[],
        );
        reg.set_pid_proc_start(4242, Some(1_000));

        let mut new_event = p0_event(
            "pid-reuse",
            ParsedEventState::State(AgentState::Running),
            200,
        );
        new_event.pid = Some(4242);
        reg.apply_event_with_meta(new_event, p0_meta("inst-new", 1), None, &[]);
        assert_eq!(
            reg.event_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            None,
            "a new instance must not inherit the old process start"
        );
        reg.set_pid_proc_start(4242, Some(2_000));
        assert_eq!(
            reg.file_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            Some(1_000)
        );
        assert_eq!(
            reg.event_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            Some(2_000)
        );

        // generation 1 has already queued its exit; generation 2 now owns the same PID.
        let mut monitor = PidMonitor::default();
        unsafe { monitor.retire_entry(4242, pid_test_entry(1, Some(1_000))) };
        monitor.entries.insert(4242, pid_test_entry(2, Some(2_000)));
        assert!(unsafe { monitor.complete_and_prune(&mut reg, 4242, 1) });
        assert_eq!(
            monitor.entries.get(&4242).map(|entry| entry.generation),
            Some(2)
        );
        assert!(reg.file_sessions.is_empty());
        assert_eq!(reg.event_sessions.len(), 1);
        assert!(!reg.file_versions.contains_key(&key));
        assert_eq!(
            reg.event_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            Some(2_000)
        );
        let current = reg.summarize_agent("codex", "Codex", &[]);
        assert_eq!(current.aggregate_state, AgentState::Running);
        assert_eq!(current.sessions[0].source, SessionSource::Event);

        // Only the new generation's own exit may unregister and prune it.
        assert!(unsafe { monitor.complete_and_prune(&mut reg, 4242, 2) });
        assert!(!monitor.entries.contains_key(&4242));
        assert!(reg.file_sessions.is_empty());
        assert!(reg.event_sessions.is_empty());
        assert!(!reg.file_versions.contains_key(&key));
        assert!(!reg.event_versions.contains_key(&key));
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 0);
    }

    #[test]
    fn new_file_instance_same_pid_does_not_inherit_old_proc_start() {
        let mut reg = AgentRegistry::default();
        let key = ("codex".to_string(), "file-pid-reuse".to_string());
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("file-pid-reuse", AgentState::Running, 100),
                p0_meta("inst-old", 1),
            )],
            None,
            &[],
        );
        reg.set_pid_proc_start(4242, Some(1_000));
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("file-pid-reuse", AgentState::Running, 200),
                p0_meta("inst-new", 1),
            )],
            None,
            &[],
        );
        assert_eq!(
            reg.file_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            None
        );
        reg.set_pid_proc_start(4242, Some(2_000));
        assert_eq!(
            reg.file_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            Some(2_000)
        );

        assert!(!reg.prune_pid_with_start(4242, Some(1_000)));
        assert!(!reg.tombstones.contains_key(&key));
        assert_eq!(reg.file_sessions.len(), 1);
        assert!(reg.prune_pid_with_start(4242, Some(2_000)));
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 0);
    }

    #[test]
    fn missing_instance_rebinds_process_start_from_os_for_ipc_and_file() {
        let mut ipc = AgentRegistry::default();
        let ipc_key = ("codex".to_string(), "legacy-ipc".to_string());
        let mut old = p0_event(
            "legacy-ipc",
            ParsedEventState::State(AgentState::Running),
            100,
        );
        old.pid = Some(4242);
        ipc.apply_event(old.clone(), None, &[]);
        ipc.set_pid_proc_start(4242, Some(1_000));
        let mut new = old;
        new.updated_at = Some(200);
        ipc.apply_event(new, None, &[]);
        // Legacy events have no producer generation proof, so the OS watcher must
        // rebind process identity instead of inheriting the previous PID's start.
        assert_eq!(
            ipc.event_versions
                .get(&ipc_key)
                .and_then(|stamp| stamp.proc_start),
            None
        );
        ipc.set_pid_proc_start(4242, Some(2_000));
        assert_eq!(
            ipc.event_versions
                .get(&ipc_key)
                .and_then(|stamp| stamp.proc_start),
            Some(2_000)
        );

        let mut files = AgentRegistry::default();
        let file_key = ("codex".to_string(), "legacy-file".to_string());
        files.replace_file_sessions(
            vec![p0_file_session("legacy-file", AgentState::Running, 100)],
            None,
            &[],
        );
        files.set_pid_proc_start(4242, Some(1_000));
        files.replace_file_sessions(
            vec![p0_file_session("legacy-file", AgentState::Running, 200)],
            None,
            &[],
        );
        assert_eq!(
            files
                .file_versions
                .get(&file_key)
                .and_then(|stamp| stamp.proc_start),
            None
        );
        files.set_pid_proc_start(4242, Some(2_000));
        assert_eq!(
            files
                .file_versions
                .get(&file_key)
                .and_then(|stamp| stamp.proc_start),
            Some(2_000)
        );

        // Exact legacy duplicates retain both the start and its binding version. A
        // reused PID's newer OS observation must not overwrite that old identity.
        let mut duplicate = p0_event(
            "legacy-ipc",
            ParsedEventState::State(AgentState::Running),
            200,
        );
        duplicate.pid = Some(4242);
        let effect = ipc.apply_event(duplicate, None, &[]);
        assert_eq!(
            ipc.event_versions
                .get(&ipc_key)
                .and_then(|stamp| stamp.proc_bind_version),
            Some((0, 200))
        );
        ipc.bind_pid_proc_start(&effect.watch.unwrap(), Some(3_000));
        assert_eq!(
            ipc.event_versions
                .get(&ipc_key)
                .and_then(|stamp| stamp.proc_start),
            Some(2_000)
        );
        assert!(ipc.prune_pid_with_start(4242, Some(2_000)));

        let (_, _, requests) = files.replace_file_sessions(
            vec![p0_file_session("legacy-file", AgentState::Running, 200)],
            None,
            &[],
        );
        assert_eq!(
            files
                .file_versions
                .get(&file_key)
                .and_then(|stamp| stamp.proc_bind_version),
            Some((0, 200))
        );
        files.bind_pid_proc_start(&requests[0], Some(3_000));
        assert_eq!(
            files
                .file_versions
                .get(&file_key)
                .and_then(|stamp| stamp.proc_start),
            Some(2_000)
        );
        assert!(files.prune_pid_with_start(4242, Some(2_000)));
    }

    #[test]
    fn ensure_watching_retires_exited_entry_before_registering_reused_pid() {
        let pid = std::process::id();
        let mut monitor = PidMonitor::default();
        let old = pid_test_entry(3, Some(1));
        let exited_handle = old.proc_handle;
        monitor.entries.insert(pid, old);
        monitor.next_generation = 7;
        unsafe { SetEvent(exited_handle) }.unwrap();

        let result =
            unsafe { monitor.ensure_watching(HWND(std::ptr::dangling_mut::<c_void>()), pid) };
        let PidWatchResult::Watching {
            proc_start,
            generation,
        } = result
        else {
            panic!("current test process should be watchable");
        };
        assert_eq!(generation, 7);
        assert!(proc_start.is_some());
        assert_eq!(monitor.retired.get(&(pid, 3)), Some(&Some(1)));
        assert_eq!(
            monitor.entries.get(&pid).map(|entry| entry.generation),
            Some(7)
        );
        assert_eq!(
            monitor.entries.get(&pid).and_then(|entry| entry.proc_start),
            proc_start
        );
        unsafe { monitor.clear() };
    }

    #[test]
    fn live_child_exit_posts_generation_and_prunes_only_the_matching_process() {
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        struct WindowGuard(HWND);
        impl Drop for WindowGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }

        let instance = unsafe { GetModuleHandleW(None) }.unwrap();
        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("Static"),
                w!("FeatherDock PID test"),
                WS_OVERLAPPED,
                0,
                0,
                1,
                1,
                HWND_MESSAGE,
                None,
                instance,
                None,
            )
        }
        .unwrap();
        let window = WindowGuard(hwnd);
        let child = Command::new("ping.exe")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = ChildGuard(child);
        let pid = child.0.id();
        let mut monitor = PidMonitor::default();
        let PidWatchResult::Watching {
            proc_start,
            generation,
        } = (unsafe { monitor.ensure_watching(hwnd, pid) })
        else {
            panic!("owned child process should be watchable");
        };
        assert!(proc_start.is_some());
        assert_eq!(
            monitor.entries.get(&pid).map(|entry| entry.generation),
            Some(generation)
        );

        let mut registry = AgentRegistry::default();
        let mut event = p0_event(
            "live-exit",
            ParsedEventState::State(AgentState::Running),
            200,
        );
        event.pid = Some(pid);
        let effect = registry.apply_event_with_meta(event, p0_meta("inst-live", 1), None, &[]);
        registry.bind_pid_watch(&effect.watch.unwrap(), proc_start, generation);
        assert_eq!(registry.event_sessions.len(), 1);

        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let received = loop {
            let mut message = MSG::default();
            if unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                if message.hwnd == hwnd && message.message == WM_AGENT_PROCESS_EXITED {
                    break Some((message.wParam.0 as u32, message.lParam.0 as usize));
                }
            } else if Instant::now() >= deadline {
                break None;
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        };
        let (received_pid, received_generation) =
            received.expect("real process callback should post to the message-only window");
        assert_eq!(received_pid, pid);
        assert_eq!(received_generation, generation);
        assert!(unsafe {
            monitor.complete_and_prune(&mut registry, received_pid, received_generation)
        });
        assert!(!monitor.entries.contains_key(&pid));
        assert!(registry.event_sessions.is_empty());
        drop(window);
    }

    #[test]
    fn stale_or_missing_completion_never_prunes_by_pid() {
        let mut reg = AgentRegistry::default();
        let mut event = p0_event(
            "missing-watch",
            ParsedEventState::State(AgentState::Running),
            200,
        );
        event.pid = Some(4242);
        reg.apply_event_with_meta(event, p0_meta("inst-new", 1), None, &[]);
        let key = ("codex".to_string(), "missing-watch".to_string());
        let mut monitor = PidMonitor::default();
        monitor.entries.insert(4242, pid_test_entry(9, None));

        assert!(!unsafe { monitor.complete_and_prune(&mut reg, 4242, 8) });
        assert!(monitor.entries.contains_key(&4242));
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 1);
        assert!(!reg.tombstones.contains_key(&key));

        // Even a matching callback without an OS process start is not a PID wildcard.
        assert!(!unsafe { monitor.complete_and_prune(&mut reg, 4242, 9) });
        assert!(!monitor.entries.contains_key(&4242));
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 1);
        assert!(!reg.tombstones.contains_key(&key));

        // A duplicate/missing completion remains a no-op as well.
        assert!(!unsafe { monitor.complete_and_prune(&mut reg, 4242, 9) });
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 1);
        assert!(!reg.tombstones.contains_key(&key));

        unsafe { monitor.retire_entry(4242, pid_test_entry(10, None)) };
        assert!(!unsafe { monitor.complete_and_prune(&mut reg, 4242, 10) });
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 1);
        assert!(!reg.tombstones.contains_key(&key));
    }

    #[test]
    fn proc_start_backfill_does_not_cross_instance_or_channel() {
        let mut reg = AgentRegistry::default();
        let key = ("codex".to_string(), "targeted-bind".to_string());
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("targeted-bind", AgentState::Running, 100),
                p0_meta("inst-old", 1),
            )],
            None,
            &[],
        );
        let mut event = p0_event(
            "targeted-bind",
            ParsedEventState::State(AgentState::Running),
            200,
        );
        event.pid = Some(4242);
        let effect = reg.apply_event_with_meta(event, p0_meta("inst-new", 1), None, &[]);
        assert_eq!(
            reg.file_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            None
        );
        assert_eq!(
            reg.event_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            None
        );

        reg.bind_pid_proc_start(&effect.watch.unwrap(), Some(2_000));
        assert_eq!(
            reg.file_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            None
        );
        assert_eq!(
            reg.event_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            Some(2_000)
        );

        // The hidden old file row survives with its own metadata, but emits no request
        // that could bind it to the newer IPC process.
        let (_, _, requests) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("targeted-bind", AgentState::Running, 100),
                p0_meta("inst-old", 1),
            )],
            None,
            &[],
        );
        assert!(requests.is_empty());
        assert_eq!(
            reg.file_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            None
        );
        assert!(!reg.prune_pid_with_start(4242, Some(1_000)));
        assert_eq!(reg.file_sessions.len(), 1);
        assert_eq!(reg.event_sessions.len(), 1);
        assert!(!reg.tombstones.contains_key(&key));
    }

    #[test]
    fn dual_channel_prune_preserves_surviving_ack_and_read_markers() {
        let mut waiting = AgentRegistry::default();
        let waiting_key = ("codex".to_string(), "waiting-survives".to_string());
        let mut event = p0_event(
            "waiting-survives",
            ParsedEventState::State(AgentState::Waiting),
            100,
        );
        event.pid = Some(4242);
        let effect = waiting.apply_event_with_meta(event, p0_meta("inst-a", 100), None, &[]);
        waiting.bind_pid_proc_start(&effect.watch.unwrap(), Some(2_000));
        let (_, _, requests) = waiting.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("waiting-survives", AgentState::Waiting, 200),
                p0_meta("inst-a", 200),
            )],
            None,
            &[],
        );
        waiting.bind_pid_proc_start(&requests[0], Some(1_000));
        waiting.mark_session_read("codex", "waiting-survives");
        assert!(waiting.waiting_bounce_acked.contains(&waiting_key));
        assert!(waiting.prune_pid_with_start(4242, Some(2_000)));
        assert!(waiting.event_sessions.is_empty());
        assert_eq!(waiting.file_sessions.len(), 1);
        assert!(waiting.waiting_bounce_acked.contains(&waiting_key));
        assert_eq!(
            waiting
                .summarize_agent("codex", "Codex", &[])
                .aggregate_state,
            AgentState::Waiting
        );
        assert!(waiting.prune_pid_with_start(4242, Some(1_000)));
        assert!(!waiting.waiting_bounce_acked.contains(&waiting_key));

        let mut read = AgentRegistry::default();
        let read_key = ("codex".to_string(), "read-survives".to_string());
        let mut event = p0_event(
            "read-survives",
            ParsedEventState::State(AgentState::Running),
            100,
        );
        event.pid = Some(4242);
        let effect = read.apply_event_with_meta(event, p0_meta("inst-a", 100), None, &[]);
        read.bind_pid_proc_start(&effect.watch.unwrap(), Some(2_000));
        let (_, _, requests) = read.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("read-survives", AgentState::Done, 200),
                p0_meta("inst-a", 200),
            )],
            None,
            &[],
        );
        read.bind_pid_proc_start(&requests[0], Some(1_000));
        assert!(read.mark_session_read("codex", "read-survives"));
        assert!(read.read_file_versions.contains_key(&read_key));
        assert!(read.prune_pid_with_start(4242, Some(2_000)));
        assert!(read.event_sessions.is_empty());
        assert_eq!(read.file_sessions[0].state, AgentState::Idle);
        assert!(read.read_file_versions.contains_key(&read_key));

        let (_, bounces, _) = read.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("read-survives", AgentState::Done, 200),
                p0_meta("inst-a", 200),
            )],
            None,
            &[],
        );
        assert!(bounces.is_empty());
        assert_eq!(read.file_sessions[0].state, AgentState::Idle);
        assert!(read.read_file_versions.contains_key(&read_key));
        assert!(read.prune_pid_with_start(4242, Some(1_000)));
        assert!(!read.read_file_versions.contains_key(&read_key));
    }

    #[test]
    fn delayed_same_file_version_does_not_rebind_reused_pid_start() {
        let pid = std::process::id();
        let key = ("codex".to_string(), "delayed-rebind".to_string());
        let mut reg = AgentRegistry::default();
        let mut event = p0_event(
            "delayed-rebind",
            ParsedEventState::State(AgentState::Running),
            100,
        );
        event.pid = Some(pid);
        let effect = reg.apply_event_with_meta(event, p0_meta("inst-a", 100), None, &[]);
        reg.bind_pid_proc_start(&effect.watch.unwrap(), Some(1_000));
        let file = p0_file_session("delayed-rebind", AgentState::Running, 200);
        let file = AgentSession {
            pid: Some(pid),
            ..file
        };
        let (_, _, requests) = reg.replace_file_sessions_with_meta(
            vec![(file.clone(), p0_meta("inst-a", 200))],
            None,
            &[],
        );
        reg.bind_pid_proc_start(&requests[0], Some(1_000));
        assert_eq!(
            reg.file_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            Some(1_000)
        );
        assert_eq!(
            reg.event_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            Some(1_000)
        );

        let (_, _, delayed_requests) = reg.replace_file_sessions_with_meta(
            vec![(file.clone(), p0_meta("inst-a", 200))],
            None,
            &[],
        );
        assert_eq!(delayed_requests.len(), 1);

        // The old watch has exited and queued gen1; the current test process now
        // supplies a different real OS start for the same numeric PID fixture.
        let mut monitor = PidMonitor::default();
        let old = pid_test_entry(1, Some(1_000));
        let exited_handle = old.proc_handle;
        monitor.entries.insert(pid, old);
        monitor.next_generation = 2;
        unsafe { SetEvent(exited_handle) }.unwrap();
        let PidWatchResult::Watching {
            proc_start: Some(new_start),
            generation: 2,
        } = (unsafe { monitor.ensure_watching(HWND(std::ptr::dangling_mut::<c_void>()), pid) })
        else {
            panic!("current test process should be watchable");
        };
        assert_ne!(new_start, 1_000);
        reg.bind_pid_proc_start(&delayed_requests[0], Some(new_start));
        assert_eq!(
            reg.file_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            Some(1_000)
        );
        assert_eq!(
            reg.event_versions
                .get(&key)
                .and_then(|stamp| stamp.proc_start),
            Some(1_000)
        );

        assert!(unsafe { monitor.complete_and_prune(&mut reg, pid, 1) });
        assert_eq!(
            monitor.entries.get(&pid).map(|entry| entry.generation),
            Some(2)
        );
        assert!(reg.file_sessions.is_empty());
        assert!(reg.event_sessions.is_empty());
        assert!(!reg.file_versions.contains_key(&key));
        assert!(!reg.event_versions.contains_key(&key));
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 0);
        unsafe { monitor.clear() };
    }

    #[test]
    fn same_process_start_prunes_both_channels_once() {
        let mut reg = AgentRegistry::default();
        let key = ("codex".to_string(), "same-start".to_string());
        let mut event = p0_event(
            "same-start",
            ParsedEventState::State(AgentState::Running),
            100,
        );
        event.pid = Some(4242);
        let effect = reg.apply_event_with_meta(event, p0_meta("inst-a", 100), None, &[]);
        reg.bind_pid_proc_start(&effect.watch.unwrap(), Some(1_000));
        let (_, _, requests) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("same-start", AgentState::Running, 200),
                p0_meta("inst-a", 200),
            )],
            None,
            &[],
        );
        reg.bind_pid_proc_start(&requests[0], Some(1_000));
        assert_eq!(reg.event_sessions.len(), 1);
        assert_eq!(reg.file_sessions.len(), 1);

        assert!(reg.prune_pid_with_start(4242, Some(1_000)));
        assert!(reg.event_sessions.is_empty());
        assert!(reg.file_sessions.is_empty());
        assert!(!reg.event_versions.contains_key(&key));
        assert!(!reg.file_versions.contains_key(&key));
        assert_eq!(reg.tombstones.len(), 1);
        assert!(!reg.prune_pid_with_start(4242, Some(1_000)));
    }

    #[test]
    fn delayed_file_snapshots_preserve_read_and_waiting_ack() {
        let mut reg = AgentRegistry::default();
        let done_key = ("codex".to_string(), "done-read".to_string());
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("done-read", AgentState::Done, 200),
                p0_meta("inst-a", 200),
            )],
            None,
            &[],
        );
        assert!(reg.mark_session_read("codex", "done-read"));
        assert_eq!(reg.file_sessions[0].state, AgentState::Idle);
        assert_eq!(
            reg.read_file_versions.get(&done_key).map(|stamp| stamp.seq),
            Some(200)
        );

        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("done-read", AgentState::Done, 100),
                p0_meta("inst-a", 100),
            )],
            None,
            &[],
        );
        assert!(bounces.is_empty());
        assert_eq!(reg.file_sessions[0].state, AgentState::Idle);
        assert_eq!(
            reg.file_versions.get(&done_key).map(|stamp| stamp.seq),
            Some(200)
        );
        assert_eq!(
            reg.read_file_versions.get(&done_key).map(|stamp| stamp.seq),
            Some(200)
        );

        let waiting_key = ("codex".to_string(), "waiting-ack".to_string());
        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![
                (
                    p0_file_session("done-read", AgentState::Done, 200),
                    p0_meta("inst-a", 200),
                ),
                (
                    p0_file_session("waiting-ack", AgentState::Waiting, 200),
                    p0_meta("inst-a", 200),
                ),
            ],
            None,
            &[],
        );
        assert_eq!(bounces, vec!["codex".to_string()]);
        reg.mark_session_read("codex", "waiting-ack");
        assert!(reg.waiting_bounce_acked.contains(&waiting_key));
        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![
                (
                    p0_file_session("done-read", AgentState::Done, 200),
                    p0_meta("inst-a", 200),
                ),
                (
                    p0_file_session("waiting-ack", AgentState::Waiting, 100),
                    p0_meta("inst-a", 100),
                ),
            ],
            None,
            &[],
        );
        assert!(bounces.is_empty());
        assert!(reg.waiting_bounce_acked.contains(&waiting_key));
        assert_eq!(
            reg.file_versions.get(&waiting_key).map(|stamp| stamp.seq),
            Some(200)
        );
    }

    #[test]
    fn newer_ipc_preserves_hidden_file_channel_and_ack() {
        let mut reg = AgentRegistry::default();
        let key = ("codex".to_string(), "shared-ack".to_string());
        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("shared-ack", AgentState::Waiting, 100),
                p0_meta("inst-a", 100),
            )],
            None,
            &[],
        );
        assert_eq!(bounces, vec!["codex".to_string()]);
        reg.mark_session_read("codex", "shared-ack");
        reg.apply_event_with_meta(
            p0_event(
                "shared-ack",
                ParsedEventState::State(AgentState::Waiting),
                200,
            ),
            p0_meta("inst-a", 200),
            None,
            &[],
        );
        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("shared-ack", AgentState::Waiting, 100),
                p0_meta("inst-a", 100),
            )],
            None,
            &[],
        );
        assert!(bounces.is_empty());
        assert_eq!(reg.file_sessions.len(), 1);
        assert_eq!(
            reg.file_versions.get(&key).map(|stamp| stamp.seq),
            Some(100)
        );
        assert!(reg.waiting_bounce_acked.contains(&key));
    }

    #[test]
    fn file_read_marker_uses_instance_and_sequence_not_only_timestamp() {
        let mut reg = AgentRegistry::default();
        let key = ("codex".to_string(), "versioned-read".to_string());
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("versioned-read", AgentState::Done, 200),
                p0_meta("inst-a", 200),
            )],
            None,
            &[],
        );
        reg.mark_session_read("codex", "versioned-read");

        // Higher seq is newer even when its wall-clock second is lower.
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("versioned-read", AgentState::Done, 100),
                p0_meta("inst-a", 201),
            )],
            None,
            &[],
        );
        assert_eq!(reg.file_sessions[0].state, AgentState::Done);
        reg.mark_session_read("codex", "versioned-read");
        assert_eq!(
            reg.read_file_versions.get(&key).map(|stamp| stamp.seq),
            Some(201)
        );

        // Same timestamp under a new producer instance is a new unread completion.
        reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("versioned-read", AgentState::Done, 100),
                p0_meta("inst-b", 1),
            )],
            None,
            &[],
        );
        assert_eq!(reg.file_sessions[0].state, AgentState::Done);
    }

    #[test]
    fn explicit_new_instance_same_second_pid_passes_dead_tombstone_for_ipc_and_file() {
        let mut ipc = AgentRegistry::default();
        let mut old = p0_event(
            "restart-ipc",
            ParsedEventState::State(AgentState::Running),
            100,
        );
        old.pid = Some(4242);
        ipc.apply_event_with_meta(old, p0_meta("old", 9), None, &[]);
        ipc.set_pid_proc_start(4242, Some(1_000));
        assert!(ipc.prune_pid_with_start(4242, Some(1_000)));

        let mut legacy = p0_event(
            "restart-ipc",
            ParsedEventState::State(AgentState::Running),
            100,
        );
        legacy.pid = Some(4242);
        assert!(ipc.apply_event(legacy, None, &[]).watch.is_none());
        let mut new = p0_event(
            "restart-ipc",
            ParsedEventState::State(AgentState::Running),
            100,
        );
        new.pid = Some(4242);
        let effect = ipc.apply_event_with_meta(new, p0_meta("new", 1), None, &[]);
        assert!(effect.changed);
        assert!(effect.watch.is_some());
        assert_eq!(
            ipc.summarize_agent("codex", "Codex", &[]).aggregate_state,
            AgentState::Running
        );

        let mut files = AgentRegistry::default();
        files.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("restart-file", AgentState::Running, 100),
                p0_meta("old", 9),
            )],
            None,
            &[],
        );
        files.set_pid_proc_start(4242, Some(1_000));
        assert!(files.prune_pid_with_start(4242, Some(1_000)));
        let (_, _, legacy_requests) = files.replace_file_sessions(
            vec![p0_file_session("restart-file", AgentState::Running, 100)],
            None,
            &[],
        );
        assert!(legacy_requests.is_empty());
        let (_, _, requests) = files.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("restart-file", AgentState::Running, 100),
                p0_meta("new", 1),
            )],
            None,
            &[],
        );
        assert_eq!(files.file_sessions.len(), 1);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            files.summarize_agent("codex", "Codex", &[]).aggregate_state,
            AgentState::Running
        );
    }

    #[test]
    fn file_waiting_reentry_uses_previous_visible_merged_state() {
        let mut reg = AgentRegistry::default();
        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("waiting-visible", AgentState::Waiting, 100),
                p0_meta("inst-a", 100),
            )],
            None,
            &[],
        );
        assert_eq!(bounces, vec!["codex".to_string()]);
        reg.apply_event_with_meta(
            p0_event(
                "waiting-visible",
                ParsedEventState::State(AgentState::Running),
                200,
            ),
            p0_meta("inst-a", 200),
            None,
            &[],
        );
        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("waiting-visible", AgentState::Waiting, 100),
                p0_meta("inst-a", 100),
            )],
            None,
            &[],
        );
        assert!(bounces.is_empty());
        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("waiting-visible", AgentState::Waiting, 300),
                p0_meta("inst-a", 300),
            )],
            None,
            &[],
        );
        assert_eq!(bounces, vec!["codex".to_string()]);
        let (_, bounces, _) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("waiting-visible", AgentState::Waiting, 300),
                p0_meta("inst-a", 300),
            )],
            None,
            &[],
        );
        assert!(bounces.is_empty());
        assert_eq!(
            reg.summarize_agent("codex", "Codex", &[]).sessions[0].source,
            SessionSource::File
        );

        // Preserve the legacy foreground rule: an already visible Waiting snapshot
        // does not gain a delayed bounce merely because focus later disappears.
        let mut foreground = AgentRegistry::default();
        let (_, bounces, _) = foreground.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("foreground-waiting", AgentState::Waiting, 100),
                p0_meta("inst-a", 100),
            )],
            Some(77),
            &["codex".to_string()],
        );
        assert!(bounces.is_empty());
        let (_, bounces, _) = foreground.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("foreground-waiting", AgentState::Waiting, 100),
                p0_meta("inst-a", 100),
            )],
            None,
            &[],
        );
        assert!(bounces.is_empty());
    }

    #[test]
    fn current_none_exit_prunes_exact_watch_generation() {
        let mut reg = AgentRegistry::default();
        let key = ("codex".to_string(), "none-generation".to_string());
        let mut event = p0_event(
            "none-generation",
            ParsedEventState::State(AgentState::Running),
            100,
        );
        event.pid = Some(4242);
        let effect = reg.apply_event_with_meta(event, p0_meta("inst-a", 1), None, &[]);
        reg.bind_pid_watch(&effect.watch.unwrap(), None, 9);
        assert_eq!(
            reg.event_versions
                .get(&key)
                .and_then(|stamp| stamp.watch_generation),
            Some(9)
        );
        let mut monitor = PidMonitor::default();
        monitor.entries.insert(4242, pid_test_entry(9, None));
        assert!(!unsafe { monitor.complete_and_prune(&mut reg, 4242, 8) });
        assert_eq!(reg.event_sessions.len(), 1);
        assert!(unsafe { monitor.complete_and_prune(&mut reg, 4242, 9) });
        assert!(reg.event_sessions.is_empty());
        assert_eq!(
            reg.tombstones
                .get(&key)
                .and_then(|tomb| tomb.stamp.watch_generation),
            Some(9)
        );
        assert!(!unsafe { monitor.complete_and_prune(&mut reg, 4242, 9) });

        // Retired None remains a no-op and cannot remove a newer current generation.
        let mut current = AgentRegistry::default();
        let mut event = p0_event(
            "none-current",
            ParsedEventState::State(AgentState::Running),
            200,
        );
        event.pid = Some(4242);
        let effect = current.apply_event_with_meta(event, p0_meta("inst-b", 1), None, &[]);
        current.bind_pid_watch(&effect.watch.unwrap(), None, 2);
        let mut monitor = PidMonitor::default();
        unsafe { monitor.retire_entry(4242, pid_test_entry(1, None)) };
        monitor.entries.insert(4242, pid_test_entry(2, None));
        assert!(!unsafe { monitor.complete_and_prune(&mut current, 4242, 1) });
        assert_eq!(current.event_sessions.len(), 1);
        assert!(monitor.entries.contains_key(&4242));
    }

    #[test]
    fn retired_none_prunes_old_file_generation_without_old_instance_revival() {
        let mut reg = AgentRegistry::default();
        let key = ("codex".to_string(), "retired-none".to_string());
        let (_, _, requests) = reg.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("retired-none", AgentState::Running, 100),
                p0_meta("old", 9),
            )],
            None,
            &[],
        );
        reg.bind_pid_watch(&requests[0], None, 1);
        let mut event = p0_event(
            "retired-none",
            ParsedEventState::State(AgentState::Running),
            100,
        );
        event.pid = Some(4242);
        let effect = reg.apply_event_with_meta(event, p0_meta("new", 1), None, &[]);
        reg.bind_pid_watch(&effect.watch.unwrap(), None, 2);
        assert_eq!(reg.file_sessions.len(), 1);
        assert_eq!(reg.event_sessions.len(), 1);

        let mut monitor = PidMonitor::default();
        unsafe { monitor.retire_entry(4242, pid_test_entry(1, None)) };
        monitor.entries.insert(4242, pid_test_entry(2, None));
        assert!(unsafe { monitor.complete_and_prune(&mut reg, 4242, 1) });
        assert!(reg.file_sessions.is_empty());
        assert_eq!(reg.event_sessions.len(), 1);
        assert_eq!(
            monitor.entries.get(&4242).map(|entry| entry.generation),
            Some(2)
        );
        assert_eq!(
            reg.summarize_agent("codex", "Codex", &[]).sessions[0].source,
            SessionSource::Event
        );

        assert!(unsafe { monitor.complete_and_prune(&mut reg, 4242, 2) });
        assert!(reg.event_sessions.is_empty());
        assert!(!reg.event_versions.contains_key(&key));
        assert!(!reg.file_versions.contains_key(&key));
        assert_eq!(reg.summarize_agent("codex", "Codex", &[]).total_sessions, 0);
    }

    #[test]
    fn watcher_load_failure_preserves_cache_snapshot_and_suppresses_ready() {
        let dir = temp_dir("watcher-file-read-failure");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("codex__cached.json");
        fs::write(
            &path,
            r#"{"agent":"codex","session_id":"cached","state":"running","pid":4242,"updated_at":100,"instance":"inst-a","seq":1}"#,
        )
        .unwrap();
        let (mut cache, snapshot) = load_sessions_preserving(&dir).unwrap();
        let cache_before = cache.clone();
        let latest = Arc::new(Mutex::new(snapshot.clone()));
        let mut registry = AgentRegistry::default();
        registry.replace_file_sessions_with_meta(snapshot, None, &[]);
        let registry_before = (
            registry.file_sessions.clone(),
            registry.file_versions.clone(),
        );
        let mut ready_count = 0;

        // Inject an error in the real per-file read path, after directory enumeration
        // and metadata have succeeded. No partial snapshot may reach the Registry.
        let failed = load_sessions_preserving_with(&dir, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected read failure",
            ))
        });
        let published = publish_loaded_sessions(failed, &mut cache, &latest, || ready_count += 1);
        if published {
            let snapshot = latest.lock().unwrap().clone();
            registry.replace_file_sessions_with_meta(snapshot, None, &[]);
        }
        assert!(!published);
        assert_eq!(ready_count, 0);
        assert_eq!(cache, cache_before);
        assert_eq!(latest.lock().unwrap().len(), 1);
        assert_eq!(registry.file_sessions, registry_before.0);
        assert_eq!(registry.file_versions, registry_before.1);

        // A later confirmed empty enumeration is authoritative and publishes one READY.
        fs::remove_file(&path).unwrap();
        let loaded = load_sessions_preserving(&dir);
        let published = publish_loaded_sessions(loaded, &mut cache, &latest, || ready_count += 1);
        if published {
            let snapshot = latest.lock().unwrap().clone();
            registry.replace_file_sessions_with_meta(snapshot, None, &[]);
        }
        assert!(published);
        assert_eq!(ready_count, 1);
        assert!(cache.is_empty());
        assert!(latest.lock().unwrap().is_empty());
        assert!(registry.file_sessions.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn watcher_publishes_post_arm_snapshot_without_a_directory_event() {
        let dir = temp_dir("watcher-post-arm-ready");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("codex__post-arm.json"),
            r#"{"agent":"codex","session_id":"post-arm","state":"running","title":"post-arm","pid":4242,"updated_at":100,"instance":"inst-a","seq":1}"#,
        )
        .unwrap();
        let window = TestWindow::new();
        let stop_event = unsafe { CreateEventW(None, true, false, None) }.unwrap();
        let latest = Arc::new(Mutex::new(Vec::new()));
        let latest_ref = Arc::clone(&latest);
        let dir_clone = dir.clone();
        let dock_raw = window.0 .0 as isize;
        let stop_raw = stop_event.0 as isize;
        let thread = thread::spawn(move || unsafe {
            watch_dir_loop_with_loader(
                HWND(dock_raw as *mut c_void),
                HANDLE(stop_raw as *mut c_void),
                &dir_clone,
                &latest_ref,
                CachedFileMap::new(),
                || load_sessions_preserving(&dir_clone),
            );
        });
        let watcher = TestWatcher {
            stop_event,
            thread: Some(thread),
        };

        // The file appeared before arm and no further write occurs. Only the forced
        // post-arm load can publish it and close the startup load/arm window.
        assert!(wait_for_test_message(window.0, WM_AGENT_SESSIONS_READY).is_some());
        let snapshot = latest.lock().unwrap().clone();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].0.session_id, "post-arm");
        let mut registry = AgentRegistry::default();
        registry.replace_file_sessions_with_meta(snapshot, None, &[]);
        assert_eq!(registry.file_sessions.len(), 1);
        drop(watcher);
        drop(window);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn watcher_notification_failure_retries_without_a_second_directory_event() {
        let dir = temp_dir("watcher-notification-retry");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("codex__retry.json");
        fs::write(
            &path,
            r#"{"agent":"codex","session_id":"retry","state":"running","title":"old","pid":4242,"updated_at":100,"instance":"inst-a","seq":1}"#,
        )
        .unwrap();
        let (mut cache, initial) = load_sessions_preserving(&dir).unwrap();
        let latest = Arc::new(Mutex::new(initial.clone()));
        let mut registry = AgentRegistry::default();
        registry.replace_file_sessions_with_meta(initial, None, &[]);
        fs::write(
            &path,
            r#"{"agent":"codex","session_id":"retry","state":"running","title":"new","pid":4242,"updated_at":200,"instance":"inst-a","seq":2}"#,
        )
        .unwrap();

        let window = TestWindow::new();
        let stop_event = unsafe { CreateEventW(None, true, false, None) }.unwrap();
        let mut attempts = 0usize;
        let mut load = || {
            attempts += 1;
            if attempts == 1 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected fail-once consumed-notification load",
                ))
            } else {
                load_sessions_preserving(&dir)
            }
        };
        // This is the production seam after a directory notification is consumed. No
        // second write/event occurs: the single delayed retry itself must publish READY.
        assert!(unsafe {
            reload_watcher_sessions(window.0, stop_event, &mut cache, &latest, &mut load)
        });
        assert_eq!(attempts, 2);
        assert!(wait_for_test_message(window.0, WM_AGENT_SESSIONS_READY).is_some());
        let snapshot = latest.lock().unwrap().clone();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].0.updated_at, 200);
        registry.replace_file_sessions_with_meta(snapshot, None, &[]);
        assert_eq!(registry.file_sessions[0].updated_at, 200);
        unsafe {
            let _ = CloseHandle(stop_event);
        }
        drop(window);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn acknowledgement_survives_duplicate_channels_and_new_waiting_realerts() {
        let mut registry = AgentRegistry::default();
        let waiting = p0_event("ack", ParsedEventState::State(AgentState::Waiting), 100);
        registry.apply_event_with_meta(waiting.clone(), p0_meta("inst-a", 1), None, &[]);
        registry.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("ack", AgentState::Waiting, 100),
                p0_meta("inst-a", 1),
            )],
            None,
            &[],
        );
        assert!(registry.acknowledge_session("codex", "ack"));
        let acknowledged = registry.summarize_agent("codex", "Codex", &[]);
        assert_eq!(acknowledged.aggregate_state, AgentState::Idle);
        assert_eq!(acknowledged.waiting_count, 0);
        assert_eq!(acknowledged.total_sessions, 1);
        assert_eq!(acknowledged.badge_text, None);
        assert_eq!(acknowledged.sessions[0].title, "ack");

        let duplicate = registry.apply_event_with_meta(waiting, p0_meta("inst-a", 1), None, &[]);
        assert!(!duplicate.changed);
        assert_eq!(duplicate.bounce_agent, None);
        assert!(duplicate.watch.is_none());
        let (changed, bounces, _) = registry.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("ack", AgentState::Waiting, 100),
                p0_meta("inst-a", 1),
            )],
            None,
            &[],
        );
        assert!(!changed);
        assert!(bounces.is_empty());
        assert_eq!(
            registry
                .summarize_agent("codex", "Codex", &[])
                .aggregate_state,
            AgentState::Idle
        );

        let newer = registry.apply_event_with_meta(
            p0_event("ack", ParsedEventState::State(AgentState::Waiting), 101),
            p0_meta("inst-a", 2),
            None,
            &[],
        );
        assert!(newer.changed);
        assert_eq!(newer.bounce_agent.as_deref(), Some("codex"));
        assert!(registry.acknowledge_session("codex", "ack"));

        let (_, file_bounces, _) = registry.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("ack", AgentState::Waiting, 102),
                p0_meta("inst-a", 3),
            )],
            None,
            &[],
        );
        assert_eq!(file_bounces, vec!["codex".to_string()]);
        assert_eq!(
            registry
                .summarize_agent("codex", "Codex", &[])
                .aggregate_state,
            AgentState::Waiting
        );
    }

    #[test]
    fn persisted_dismissal_blocks_old_channels_after_restart_and_allows_newer_event() {
        let mut registry = AgentRegistry::default();
        registry.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("clear", AgentState::Waiting, 100),
                p0_meta("inst-a", 1),
            )],
            None,
            &[],
        );
        registry.apply_event_with_meta(
            p0_event("clear", ParsedEventState::State(AgentState::Waiting), 200),
            p0_meta("inst-a", 2),
            None,
            &[],
        );
        let record = registry.dismissal_record("codex", "clear").unwrap();
        assert_eq!(record.version.seq, 2);
        assert_eq!(record.version.updated_at, 200);

        let dir = temp_dir("dismissal-restart");
        let path = dismissal_path_from_dir(&dir, "codex", "clear");
        crate::atomic::write(&path, dismissal_json(&record).as_bytes()).unwrap();
        let records = load_session_dismissals_from_dir(&dir).unwrap();
        assert_eq!(records, vec![record]);
        fs::write(dir.join("damaged.json"), "{").unwrap();
        assert!(load_session_dismissals_from_dir(&dir).is_err());
        fs::remove_file(dir.join("damaged.json")).unwrap();

        let (changed, released) = registry.dismiss_session("codex", "clear");
        assert!(changed);
        assert_eq!(released, vec![4242]);
        let stale_event = registry.apply_event_with_meta(
            p0_event("clear", ParsedEventState::State(AgentState::Waiting), 200),
            p0_meta("inst-a", 2),
            None,
            &[],
        );
        assert!(!stale_event.changed);
        assert!(stale_event.watch.is_none());
        assert_eq!(stale_event.bounce_agent, None);

        let mut restarted = AgentRegistry::default();
        restarted.restore_dismissals(records);
        let replay = restarted.apply_event_with_meta(
            p0_event("clear", ParsedEventState::State(AgentState::Waiting), 200),
            p0_meta("inst-a", 2),
            None,
            &[],
        );
        assert!(!replay.changed);
        let (changed, bounces, _) = restarted.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("clear", AgentState::Waiting, 100),
                p0_meta("inst-a", 1),
            )],
            None,
            &[],
        );
        assert!(!changed);
        assert!(bounces.is_empty());
        assert_eq!(
            restarted
                .summarize_agent("codex", "Codex", &[])
                .total_sessions,
            0
        );

        let newer = restarted.apply_event_with_meta(
            p0_event("clear", ParsedEventState::State(AgentState::Waiting), 201),
            p0_meta("inst-a", 3),
            None,
            &[],
        );
        assert!(newer.changed);
        assert_eq!(newer.bounce_agent.as_deref(), Some("codex"));
        let (changed, _, _) = restarted.replace_file_sessions_with_meta(
            vec![(
                p0_file_session("clear", AgentState::Waiting, 100),
                p0_meta("inst-a", 1),
            )],
            None,
            &[],
        );
        assert!(!changed);
        assert_eq!(
            restarted
                .summarize_agent("codex", "Codex", &[])
                .aggregate_state,
            AgentState::Waiting
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dismissal_loader_distinguishes_empty_from_directory_and_read_failures() {
        let dir = temp_dir("dismissal-io");
        assert!(load_session_dismissals_from_dir(&dir).unwrap().is_empty());
        let record = SessionDismissal {
            agent: "codex".into(),
            session_id: "locked".into(),
            version: SessionVersion {
                instance: Some("inst-a".into()),
                seq: 4,
                updated_at: 100,
            },
        };
        let path = dismissal_path_from_dir(&dir, "codex", "locked");
        crate::atomic::write(&path, dismissal_json(&record).as_bytes()).unwrap();

        use std::os::windows::fs::OpenOptionsExt;
        let locked = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert!(load_session_dismissals_from_dir(&dir).is_err());
        drop(locked);
        assert_eq!(
            load_session_dismissals_from_dir(&dir).unwrap(),
            vec![record]
        );

        let not_a_directory = temp_dir("dismissal-not-directory");
        fs::write(&not_a_directory, "file").unwrap();
        assert!(load_session_dismissals_from_dir(&not_a_directory).is_err());
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_file(not_a_directory);
    }

    #[test]
    fn stale_expected_version_is_rejected_after_same_second_sequence_advance() {
        let mut registry = AgentRegistry::default();
        registry.apply_event_with_meta(
            p0_event(
                "expected",
                ParsedEventState::State(AgentState::Waiting),
                100,
            ),
            p0_meta("inst-a", 1),
            None,
            &[],
        );
        let stale = registry.session_version("codex", "expected").unwrap();
        registry.apply_event_with_meta(
            p0_event(
                "expected",
                ParsedEventState::State(AgentState::Waiting),
                100,
            ),
            p0_meta("inst-a", 2),
            None,
            &[],
        );
        let current = registry.session_version("codex", "expected").unwrap();
        assert_ne!(stale, current);
        let summary = registry.summarize_agent("codex", "Codex", &[]);
        assert_eq!(
            summary.versions.get(&("codex".into(), "expected".into())),
            Some(&current)
        );
    }

    #[test]
    fn event_read_marker_is_released_with_its_channel() {
        let mut registry = AgentRegistry::default();
        let mut event = p0_event(
            "prune-read",
            ParsedEventState::State(AgentState::Waiting),
            100,
        );
        event.hwnd = Some(77);
        registry.apply_event_with_meta(event, p0_meta("inst-a", 1), None, &[]);
        assert!(registry.acknowledge_session("codex", "prune-read"));
        let key = ("codex".to_string(), "prune-read".to_string());
        assert!(registry.read_event_versions.contains_key(&key));
        assert!(registry.prune_dead_windows(|_| false));
        assert!(!registry.read_event_versions.contains_key(&key));
    }

    #[test]
    fn unwatch_then_rewatch_ignores_the_old_queued_generation() {
        let pid = std::process::id();
        let hwnd = HWND(std::ptr::dangling_mut::<c_void>());
        let mut monitor = PidMonitor::default();
        let mut registry = AgentRegistry::default();
        let old_generation = match unsafe { monitor.ensure_watching(hwnd, pid) } {
            PidWatchResult::Watching { generation, .. } => generation,
            other => panic!("expected current-process watch, got {other:?}"),
        };
        assert!(unsafe { monitor.unwatch(pid) });
        assert!(!unsafe { monitor.unwatch(pid) });
        let new_generation = match unsafe { monitor.ensure_watching(hwnd, pid) } {
            PidWatchResult::Watching { generation, .. } => generation,
            other => panic!("expected replacement watch, got {other:?}"),
        };
        assert_ne!(old_generation, new_generation);
        assert!(!unsafe { monitor.complete_and_prune(&mut registry, pid, old_generation) });
        assert_eq!(
            monitor.entries.get(&pid).map(|entry| entry.generation),
            Some(new_generation)
        );
        assert!(unsafe { monitor.unwatch(pid) });
    }

    #[test]
    fn select_agent_click_action_prioritizes_single_waiting_session() {
        let summary = AgentDockSummary {
            agent_id: "codex".into(),
            display_name: "Codex".into(),
            aggregate_state: AgentState::Waiting,
            total_sessions: 2,
            active_count: 1,
            waiting_count: 1,
            error_count: 0,
            done_count: 0,
            badge_text: Some("1!".into()),
            sessions: vec![
                AgentSession {
                    agent: "codex".into(),
                    session_id: "s1".into(),
                    title: "A".into(),
                    detail: String::new(),
                    cwd: None,
                    state: AgentState::Running,
                    hwnd: Some(1),
                    pid: None,
                    deep_link: None,
                    updated_at: 10,
                    source: SessionSource::Event,
                },
                AgentSession {
                    agent: "codex".into(),
                    session_id: "s2".into(),
                    title: "B".into(),
                    detail: String::new(),
                    cwd: None,
                    state: AgentState::Waiting,
                    hwnd: Some(2),
                    pid: None,
                    deep_link: None,
                    updated_at: 11,
                    source: SessionSource::Event,
                },
            ],
            versions: HashMap::new(),
        };
        assert_eq!(
            select_agent_click_action(&summary),
            AgentClickAction::JumpSession(1)
        );
    }
}
