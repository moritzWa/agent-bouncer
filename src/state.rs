use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_ROTATE_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Settings {
    pub slots: usize,
    pub min_free_gb: f64,
    pub max_cpu_busy: f64,
}

impl Settings {
    pub fn from_env() -> Self {
        let cores = crate::sysinfo::cores();
        let default_slots = (cores / 6).clamp(2, 6);
        Settings {
            slots: env_parse("AGENT_BOUNCER_SLOTS")
                .filter(|n: &usize| *n >= 1)
                .or_else(recorded_slots)
                .unwrap_or(default_slots),
            min_free_gb: env_parse("AGENT_BOUNCER_MIN_FREE_GB").unwrap_or(3.0),
            max_cpu_busy: env_parse("AGENT_BOUNCER_MAX_CPU").unwrap_or(95.0),
        }
    }
}

fn recorded_slots() -> Option<usize> {
    let text = fs::read_to_string(root().join("slot-count")).ok()?;
    text.trim().parse().ok().filter(|n| *n >= 1)
}

/// Remember the slot count a job ran with, so `status` run from another shell
/// shows the same total even without the environment variable.
pub fn record_slots(root: &Path, slots: usize) {
    if std::env::var_os("AGENT_BOUNCER_SLOTS").is_some() {
        write_atomic(&root.join("slot-count"), slots.to_string().as_bytes());
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

pub fn root() -> PathBuf {
    if let Ok(dir) = std::env::var("AGENT_BOUNCER_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(state) = std::env::var("XDG_STATE_HOME") {
        if !state.is_empty() {
            return PathBuf::from(state).join("agent-bouncer");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/state/agent-bouncer")
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

pub fn local_day(ms: u64) -> String {
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

pub fn fmt_dur(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0 => format!("{ms}ms"),
        1..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

pub fn fmt_mb(mb: u64) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

pub fn ensure_dirs(root: &Path) -> std::io::Result<()> {
    fs::create_dir_all(root.join("slots"))?;
    fs::create_dir_all(root.join("waiting"))
}

pub fn flock(file: &File, op: i32) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), op) == 0 }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if fs::write(&tmp, bytes).is_ok() && fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Event {
    pub ts: u64,
    pub ev: String,
    pub job: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dur_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leftover_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_ms: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avail_gb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_gb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_busy: Option<f64>,
}

impl Event {
    pub fn new(ev: &str, job: &str) -> Self {
        Event { ts: now_ms(), ev: ev.into(), job: job.into(), ..Default::default() }
    }
}

pub fn log_event(root: &Path, event: &Event) {
    let path = root.join("events.jsonl");
    if fs::metadata(&path).map(|m| m.len() > LOG_ROTATE_BYTES).unwrap_or(false) {
        let _ = fs::rename(&path, root.join("events.1.jsonl"));
    }
    let Ok(mut line) = serde_json::to_string(event) else { return };
    line.push('\n');
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

pub fn read_events(root: &Path) -> Vec<Event> {
    let mut out = Vec::new();
    for name in ["events.1.jsonl", "events.jsonl"] {
        if let Ok(text) = fs::read_to_string(root.join(name)) {
            out.extend(text.lines().filter_map(|l| serde_json::from_str::<Event>(l).ok()));
        }
    }
    out
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SlotInfo {
    pub job: String,
    pub pid: u32,
    pub label: Option<String>,
    pub cmd: String,
    pub cwd: String,
    pub started_ms: u64,
    pub weight: usize,
    #[serde(default)]
    pub rss_mb: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WaitInfo {
    pub job: String,
    pub pid: u32,
    pub label: Option<String>,
    pub cmd: String,
    pub cwd: String,
    pub since_ms: u64,
    pub weight: usize,
    pub reason: String,
}
