//! Agentic Dock state store, GUI Agent identification, zero-dep event JSON parser,
//! `--agent-event` (`WM_COPYDATA`) IPC, `%LOCALAPPDATA%\FeatherDock\agent-sessions`
//! kernel-event watcher, and zombie lifecycle cleanup.

use std::collections::{HashMap, HashSet};
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
const MAX_ID_CHARS: usize = 96;
const MAX_INSTANCE_CHARS: usize = 64;
const MAX_TITLE_CHARS: usize = 120;
const MAX_DETAIL_CHARS: usize = 240;
const MAX_PATH_CHARS: usize = 512;
const MAX_DEEP_LINK_CHARS: usize = 512;
const IPC_TIMEOUT_MS: u32 = 2000;
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventEffect {
    pub changed: bool,
    pub bounce_agent: Option<String>,
    pub watch_pid: Option<u32>,
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
    event_sessions: Vec<AgentSession>,
    file_sessions: Vec<AgentSession>,
    read_file_versions: HashMap<(String, String), u64>,
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

    pub fn set_pid_proc_start(&mut self, pid: u32, proc_start: Option<u64>) {
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
                watch_pid: None,
            };
        };
        let Some(session_id) = sanitize_id(&payload.session_id, MAX_ID_CHARS) else {
            return EventEffect {
                changed: false,
                bounce_agent: None,
                watch_pid: None,
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
        let inherited_proc_start = existing_stamp
            .as_ref()
            .filter(|s| s.pid.is_some() && s.pid == pid_final)
            .and_then(|s| s.proc_start);
        let incoming_stamp = VersionStamp {
            updated_at: incoming_ts,
            seq: meta.seq.unwrap_or(0),
            instance: meta.instance.as_deref().and_then(sanitize_instance),
            pid: pid_final,
            proc_start: inherited_proc_start,
        };

        // Every event, including Closed, passes the same tombstone and latest-version gate
        // before it can mutate sessions or produce user-visible side effects.
        if let Some(tomb) = self.tombstones.get(&key) {
            if is_blocked_by_tombstone(tomb, &incoming_stamp) {
                return EventEffect {
                    changed: false,
                    bounce_agent: None,
                    watch_pid: None,
                };
            }
        }
        if let Some(ref existing) = existing_stamp {
            if is_older_stamp(existing, &incoming_stamp) {
                return EventEffect {
                    changed: false,
                    bounce_agent: None,
                    watch_pid: None,
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
            self.read_file_versions.remove(&key);
            self.waiting_bounce_acked.remove(&key);
            self.dead_hwnds.remove(&key);
            return EventEffect {
                changed: self.event_sessions.len() != before_event
                    || self.file_sessions.len() != before_file,
                bounce_agent: None,
                watch_pid: None,
            };
        };

        self.tombstones.remove(&key);

        let payload_hwnd = payload.hwnd.filter(|&h| h != 0);
        if payload_hwnd.is_some() {
            self.dead_hwnds.remove(&key);
        }
        let session_hwnd = payload_hwnd.or_else(|| prev.as_ref().and_then(|p| p.hwnd));
        let session_fg = is_session_foreground(&agent, session_hwnd, foreground_hwnd, fg_agents);
        if state == AgentState::Done && session_fg {
            state = AgentState::Idle;
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
            watch_pid: pid,
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
    ) -> (bool, Vec<String>, Vec<u32>) {
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
    ) -> (bool, Vec<String>, Vec<u32>) {
        let old = std::mem::replace(&mut self.file_sessions, Vec::with_capacity(incoming.len()));
        let mut bounces = Vec::new();
        let mut pids = Vec::new();
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

            if let Some(event_stamp) = self.event_versions.get(&key).cloned().or_else(|| {
                self.event_sessions
                    .iter()
                    .find(|s| s.agent == key.0 && s.session_id == key.1)
                    .map(|s| Self::session_stamp(s, None))
            }) {
                // `is_older_stamp(&stamp, event_stamp)` is true precisely when the file is
                // strictly newer. Equal versions belong to IPC by deterministic tie-break.
                if !is_older_stamp(&stamp, &event_stamp) {
                    continue;
                }
            }

            let prev = old
                .iter()
                .find(|s| s.agent == session.agent && s.session_id == session.session_id);
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
                        if let Some(pid) = previous.pid.filter(|&p| p != 0) {
                            pids.push(pid);
                        }
                        self.file_sessions.push(previous.clone());
                    }
                    continue;
                }
            }

            self.tombstones.remove(&key);
            if let Some(prev_start) = self
                .file_versions
                .get(&key)
                .filter(|prev| prev.pid.is_some() && prev.pid == stamp.pid)
                .and_then(|prev| prev.proc_start)
            {
                stamp.proc_start = Some(prev_start);
            }

            if let Some(dead) = self.dead_hwnds.get(&key) {
                if session.hwnd == Some(dead.hwnd) && !is_older_stamp(&stamp, &dead.stamp) {
                    session.hwnd = None;
                }
            }

            let session_fg =
                is_session_foreground(&session.agent, session.hwnd, foreground_hwnd, fg_agents);
            if session.state == AgentState::Done {
                let already_read = self
                    .read_file_versions
                    .get(&key)
                    .is_some_and(|&ver| ver >= session.updated_at);
                if already_read || (session_fg && prev.is_none_or(|p| p.state != AgentState::Done))
                {
                    session.state = AgentState::Idle;
                    self.read_file_versions
                        .insert(key.clone(), session.updated_at);
                }
            } else {
                self.read_file_versions.remove(&key);
            }

            if session.state != AgentState::Waiting {
                self.waiting_bounce_acked.remove(&key);
            } else if prev.is_none_or(|p| p.state != AgentState::Waiting)
                && !session_fg
                && self.waiting_bounce_acked.insert(key.clone())
            {
                bounces.push(session.agent.clone());
            }

            if let Some(pid) = session.pid.filter(|&p| p != 0) {
                pids.push(pid);
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
        (changed, bounces, pids)
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
                    s.state = AgentState::Idle;
                    self.read_file_versions
                        .insert((s.agent.clone(), s.session_id.clone()), s.updated_at);
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
                s.state = AgentState::Idle;
                self.read_file_versions
                    .insert((s.agent.clone(), s.session_id.clone()), s.updated_at);
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
                    s.state = AgentState::Idle;
                    self.read_file_versions
                        .insert((s.agent.clone(), s.session_id.clone()), s.updated_at);
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
            self.waiting_bounce_acked.remove(&key);
            self.read_file_versions.remove(&key);
            if !self
                .event_sessions
                .iter()
                .any(|s| s.agent == key.0 && s.session_id == key.1)
            {
                self.event_versions.remove(&key);
            }
            if !self
                .file_sessions
                .iter()
                .any(|s| s.agent == key.0 && s.session_id == key.1)
            {
                self.file_versions.remove(&key);
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
                if let (Some(expected), Some(recorded)) = (proc_start, stamp.proc_start) {
                    if expected != recorded {
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
            self.waiting_bounce_acked.remove(&key);
            self.read_file_versions.remove(&key);
            self.dead_hwnds.remove(&key);
            if !self
                .event_sessions
                .iter()
                .any(|s| s.agent == key.0 && s.session_id == key.1)
            {
                self.event_versions.remove(&key);
            }
            if !self
                .file_sessions
                .iter()
                .any(|s| s.agent == key.0 && s.session_id == key.1)
            {
                self.file_versions.remove(&key);
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
    load_sessions_with_meta_from_dir(dir)
        .into_iter()
        .map(|(s, _)| s)
        .collect()
}

pub fn load_sessions_with_meta_from_dir(dir: &Path) -> Vec<(AgentSession, EventMeta)> {
    let empty = HashMap::new();
    load_sessions_preserving(dir, &empty)
        .map(|(_, sessions)| sessions)
        .unwrap_or_default()
}

type CachedFileMap = HashMap<PathBuf, (AgentSession, EventMeta)>;

fn load_sessions_preserving(
    dir: &Path,
    prev: &CachedFileMap,
) -> std::io::Result<(CachedFileMap, Vec<(AgentSession, EventMeta)>)> {
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
        let Ok(meta) = entry.metadata() else {
            if let Some(cached) = prev.get(&path).cloned() {
                next_cache.insert(path, cached.clone());
                out.push(cached);
            }
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_PAYLOAD_BYTES as u64 {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(err)
                if err.kind() == std::io::ErrorKind::PermissionDenied
                    || matches!(err.raw_os_error(), Some(32 | 33)) =>
            {
                // Keep last valid snapshot across transient sharing/write locks on Windows.
                if let Some(cached) = prev.get(&path).cloned() {
                    next_cache.insert(path, cached.clone());
                    out.push(cached);
                }
                continue;
            }
            Err(_) => continue,
        };
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
    dir: PathBuf,
}

impl SessionDirWatcher {
    pub unsafe fn start(dock_hwnd: HWND) -> Self {
        let dir = sessions_dir();
        let _ = fs::create_dir_all(&dir);
        let empty = HashMap::new();
        let (init_cache, init_sessions) =
            load_sessions_preserving(&dir, &empty).unwrap_or_default();
        let latest = Arc::new(Mutex::new(init_sessions));
        let stop_event = CreateEventW(None, true, false, None).unwrap_or_default();
        if stop_event.is_invalid() {
            return Self {
                stop_event: HANDLE::default(),
                thread: None,
                latest,
                dir,
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
            dir,
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

    #[allow(dead_code)]
    pub fn refresh_now(&self) -> Vec<AgentSession> {
        let empty = HashMap::new();
        let Ok((_, fresh)) = load_sessions_preserving(&self.dir, &empty) else {
            return self.take_snapshot();
        };
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = fresh.clone();
        fresh.into_iter().map(|(s, _)| s).collect()
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

unsafe fn watch_dir_loop(
    dock_hwnd: HWND,
    stop_event: HANDLE,
    dir: &Path,
    latest: &Arc<Mutex<Vec<(AgentSession, EventMeta)>>>,
    mut cached_files: CachedFileMap,
) {
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
    let filter =
        FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE;

    loop {
        let _ = ResetEvent(io_event);
        let mut overlapped = OVERLAPPED {
            hEvent: io_event,
            ..Default::default()
        };
        if ReadDirectoryChangesW(
            dir_handle,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as u32,
            false,
            filter,
            None,
            Some(&mut overlapped),
            None,
        )
        .is_err()
        {
            break;
        }

        let wait = WaitForMultipleObjects(&[stop_event, io_event], false, INFINITE);
        if wait == WAIT_OBJECT_0 {
            let _ = CancelIoEx(dir_handle, Some(&overlapped));
            break;
        }
        if wait.0 != WAIT_OBJECT_0.0 + 1 {
            let _ = CancelIoEx(dir_handle, Some(&overlapped));
            break;
        }

        let mut bytes = 0u32;
        if GetOverlappedResult(dir_handle, &overlapped, &mut bytes, false).is_err() {
            break;
        }

        // Briefly coalesce bursts of atomic temp-file writes/renames into one reload.
        if WaitForSingleObject(stop_event, 60) == WAIT_OBJECT_0 {
            break;
        }
        if let Ok((next_cache, snapshot)) = load_sessions_preserving(dir, &cached_files) {
            cached_files = next_cache;
            *latest.lock().unwrap_or_else(|e| e.into_inner()) = snapshot;
            if !dock_hwnd.is_invalid() {
                let _ = PostMessageW(dock_hwnd, WM_AGENT_SESSIONS_READY, WPARAM(0), LPARAM(0));
            }
        }
        // A failed enumeration keeps both the cache and last published snapshot. The next
        // directory event retries; only a successful empty enumeration confirms deletion.
    }

    let _ = CloseHandle(io_event);
    let _ = CloseHandle(dir_handle);
}

// ---------- Kernel-event PID exit monitor (`RegisterWaitForSingleObject`) ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PidWatchResult {
    Watching { proc_start: Option<u64> },
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
}

struct PidWaitContext {
    dock_raw: isize,
    pid: u32,
}

#[derive(Default)]
pub struct PidMonitor {
    entries: HashMap<u32, PidWaitEntry>,
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
                };
            }
            if let Some(old) = self.entries.remove(&pid) {
                cleanup_pid_entry(old);
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
        let ctx = Box::into_raw(Box::new(PidWaitContext {
            dock_raw: dock_hwnd.0 as isize,
            pid,
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
            },
        );
        PidWatchResult::Watching { proc_start }
    }

    pub unsafe fn complete(&mut self, pid: u32) -> Option<u64> {
        self.entries.remove(&pid).and_then(|entry| {
            let start = entry.proc_start;
            cleanup_pid_entry(entry);
            start
        })
    }

    pub unsafe fn clear(&mut self) {
        for (_, entry) in self.entries.drain() {
            cleanup_pid_entry(entry);
        }
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
            LPARAM(0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("featherdock-agent-{nonce}-{name}"))
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
        assert_eq!(
            parsed.cwd.as_deref(),
            Some(r"C:\Work\Dock")
        );
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

        let empty = CachedFileMap::new();
        let (cache, snapshot) = load_sessions_preserving(&dir, &empty).unwrap();
        let mut reg = AgentRegistry::default();
        reg.replace_file_sessions_with_meta(snapshot, None, &[]);
        assert!(reg.prune_pid(4242));
        assert!(reg
            .tombstones
            .contains_key(&("codex".into(), "file1".into())));

        fs::remove_file(&path).unwrap();
        let (_, confirmed_empty) = load_sessions_preserving(&dir, &cache).unwrap();
        assert!(confirmed_empty.is_empty());
        reg.replace_file_sessions_with_meta(confirmed_empty, None, &[]);
        let key = ("codex".to_string(), "file1".to_string());
        assert_eq!(
            reg.tombstones.get(&key).map(|tomb| tomb.kind),
            Some(TombstoneKind::DeadLifecycle)
        );

        // The exact same producer version reappears after a confirmed missing-file snapshot.
        fs::write(&path, json).unwrap();
        let (_, reappeared) = load_sessions_preserving(&dir, &cache).unwrap();
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
        let empty = CachedFileMap::new();
        let (cache, snapshot) = load_sessions_preserving(&dir, &empty).unwrap();
        assert_eq!(snapshot.len(), 1);

        fs::remove_dir_all(&dir).unwrap();
        assert!(load_sessions_preserving(&dir, &cache).is_err());
        assert_eq!(cache.len(), 1);

        // Only a successful enumeration may publish an empty list and clear live file rows.
        fs::create_dir_all(&dir).unwrap();
        let (_, confirmed_empty) = load_sessions_preserving(&dir, &cache).unwrap();
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
        };
        assert_eq!(
            select_agent_click_action(&summary),
            AgentClickAction::JumpSession(1)
        );
    }
}
