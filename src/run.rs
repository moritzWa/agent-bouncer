use crate::state::*;
use crate::sysinfo;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    SIGNAL.store(sig, Ordering::SeqCst);
}

pub struct RunOpts {
    pub command: Vec<String>,
    pub wait: bool,
    pub max_wait_secs: u64,
    pub weight: usize,
    pub label: Option<String>,
    pub reap: bool,
}

pub const EXIT_BUSY: i32 = 75;
pub const EXIT_USAGE: i32 = 64;

#[derive(Default, Clone, Copy)]
struct Snapshot {
    avail_gb: Option<f64>,
    swap_gb: Option<f64>,
    cpu: Option<f64>,
}

type Admitted = (Vec<(usize, File)>, Snapshot);

pub struct Waiter {
    dir: PathBuf,
    name: String,
    _lock: File,
    info: WaitInfo,
}

impl Waiter {
    fn create(root: &Path, info: WaitInfo) -> std::io::Result<Self> {
        let dir = root.join("waiting");
        let name = format!("{:020}-{}", now_nanos(), std::process::id());
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join(format!("{name}.lock")))?;
        flock(&lock, libc::LOCK_EX);
        let waiter = Waiter { dir, name, _lock: lock, info };
        waiter.write_info();
        Ok(waiter)
    }

    fn write_info(&self) {
        if let Ok(json) = serde_json::to_vec(&self.info) {
            write_atomic(&self.dir.join(format!("{}.json", self.name)), &json);
        }
    }

    fn set_reason(&mut self, reason: &str) {
        self.info.reason = reason.into();
        self.write_info();
    }

    fn is_head(&self) -> bool {
        let mine = format!("{}.lock", self.name);
        let Ok(entries) = fs::read_dir(&self.dir) else { return true };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".lock"))
            .collect();
        names.sort();
        for name in names {
            if name == mine {
                return true;
            }
            if waiter_alive(&self.dir, &name) {
                return false;
            }
        }
        true
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.dir.join(format!("{}.json", self.name)));
        let _ = fs::remove_file(self.dir.join(format!("{}.lock", self.name)));
    }
}

fn waiter_alive(dir: &Path, lock_name: &str) -> bool {
    let born: u128 = lock_name.split('-').next().and_then(|s| s.parse().ok()).unwrap_or(0);
    if now_nanos().saturating_sub(born) < 1_000_000_000 {
        return true;
    }
    let path = dir.join(lock_name);
    let Ok(file) = File::open(&path) else { return false };
    if flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
        let _ = fs::remove_file(dir.join(lock_name.replace(".lock", ".json")));
        let _ = fs::remove_file(&path);
        false
    } else {
        true
    }
}

pub fn slot_lock_path(root: &Path, i: usize) -> PathBuf {
    root.join("slots").join(format!("slot-{i}.lock"))
}

pub fn slot_info_path(root: &Path, i: usize) -> PathBuf {
    root.join("slots").join(format!("slot-{i}.json"))
}

fn try_acquire(root: &Path, slots: usize, weight: usize) -> Option<Vec<(usize, File)>> {
    let mut held = Vec::new();
    for i in 0..slots {
        if held.len() == weight {
            break;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(slot_lock_path(root, i))
            .ok()?;
        if flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            held.push((i, file));
        }
    }
    (held.len() == weight).then_some(held)
}

fn machine_check(settings: &Settings) -> Result<Snapshot, (&'static str, Snapshot)> {
    let mut snap = Snapshot::default();
    if let Some(mem) = sysinfo::memory() {
        snap.avail_gb = Some(mem.avail_gb);
        snap.swap_gb = Some(mem.swap_used_gb);
        if mem.avail_gb < settings.min_free_gb {
            return Err(("ram", snap));
        }
    }
    if settings.max_cpu_busy < 100.0 {
        if let Some(cpu) = sysinfo::cpu_busy_pct() {
            snap.cpu = Some(cpu);
            if cpu > settings.max_cpu_busy {
                return Err(("cpu", snap));
            }
        }
    }
    Ok(snap)
}

fn explain(reason: &str) -> &'static str {
    match reason {
        "queue" => "other jobs are ahead in line",
        "slots" => "all slots are in use",
        "ram" => "free RAM is below the floor",
        "cpu" => "the CPU is saturated",
        _ => "busy",
    }
}

fn install_signal_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            libc::signal(sig, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
    }
}

fn exec_directly(command: &[String]) -> i32 {
    let err = Command::new(&command[0]).args(&command[1..]).exec();
    eprintln!("agent-bouncer: cannot run {}: {err}", command[0]);
    127
}

fn group_alive(pgid: i32) -> bool {
    unsafe { libc::kill(-pgid, 0) == 0 }
}

fn reap_group(pgid: i32) {
    if !group_alive(pgid) {
        return;
    }
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && group_alive(pgid) {
        std::thread::sleep(Duration::from_millis(100));
    }
    if group_alive(pgid) {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

fn write_slots(root: &Path, held: &[(usize, File)], info: &SlotInfo) {
    if let Ok(json) = serde_json::to_vec(info) {
        for (i, _) in held {
            write_atomic(&slot_info_path(root, *i), &json);
        }
    }
}

pub fn run(opts: RunOpts) -> i32 {
    if std::env::var_os("AGENT_BOUNCER_JOB").is_some() {
        return exec_directly(&opts.command);
    }
    let settings = Settings::from_env();
    if opts.weight == 0 || opts.weight > settings.slots {
        eprintln!(
            "agent-bouncer: --weight must be between 1 and the slot count ({})",
            settings.slots
        );
        return EXIT_USAGE;
    }
    let root = root();
    if let Err(err) = ensure_dirs(&root) {
        eprintln!("agent-bouncer: cannot create {}: {err}", root.display());
        return 1;
    }
    install_signal_handlers();

    let pid = std::process::id();
    let job = format!("{:x}-{pid}", now_ms());
    let cmd_text = opts.command.join(" ");
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let mut waiter = match Waiter::create(
        &root,
        WaitInfo {
            job: job.clone(),
            pid,
            label: opts.label.clone(),
            cmd: cmd_text.clone(),
            cwd: cwd.clone(),
            since_ms: now_ms(),
            weight: opts.weight,
            reason: "queue".into(),
        },
    ) {
        Ok(w) => w,
        Err(err) => {
            eprintln!("agent-bouncer: cannot enqueue: {err}");
            return 1;
        }
    };

    let event = |name: &str| {
        let mut e = Event::new(name, &job);
        e.label = opts.label.clone();
        e.cmd = Some(cmd_text.clone());
        e.cwd = Some(cwd.clone());
        e.weight = Some(opts.weight);
        e
    };
    log_event(&root, &event("enqueue"));

    let started = Instant::now();
    let mut last_tick = Instant::now();
    let mut blocked: BTreeMap<String, u64> = BTreeMap::new();
    let mut reason = String::new();

    let (held, admit_snap) = loop {
        let tick = last_tick.elapsed().as_millis() as u64;
        last_tick = Instant::now();
        if !reason.is_empty() {
            *blocked.entry(reason.clone()).or_default() += tick;
        }
        let sig = SIGNAL.load(Ordering::SeqCst);
        if sig != 0 {
            return 128 + sig;
        }

        let mut snap = Snapshot::default();
        let attempt: Result<Admitted, &'static str> = if !waiter.is_head() {
            Err("queue")
        } else {
            match try_acquire(&root, settings.slots, opts.weight) {
                None => Err("slots"),
                Some(held) => match machine_check(&settings) {
                    Ok(s) => Ok((held, s)),
                    Err((why, s)) => {
                        snap = s;
                        Err(why)
                    }
                },
            }
        };
        match attempt {
            Ok(pair) => break pair,
            Err(why) => {
                if reason != why {
                    reason = why.into();
                    waiter.set_reason(why);
                    let mut e = event("block");
                    e.avail_gb = snap.avail_gb;
                    e.swap_gb = snap.swap_gb;
                    e.cpu_busy = snap.cpu;
                    e.reason = Some(why.into());
                    log_event(&root, &e);
                    if opts.wait {
                        eprintln!("agent-bouncer: waiting, {}", explain(why));
                    }
                }
                let waited = started.elapsed();
                if !opts.wait || (opts.max_wait_secs > 0 && waited.as_secs() >= opts.max_wait_secs) {
                    let mut e = event("timeout");
                    e.wait_ms = Some(waited.as_millis() as u64);
                    e.blocked_ms = Some(blocked.clone());
                    log_event(&root, &e);
                    eprintln!(
                        "agent-bouncer: busy, {} (exit {EXIT_BUSY}){}",
                        explain(why),
                        if opts.wait { format!(" after {}s", waited.as_secs()) } else { String::new() }
                    );
                    return EXIT_BUSY;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };

    let wait_ms = started.elapsed().as_millis() as u64;
    drop(waiter);

    let mut info = SlotInfo {
        job: job.clone(),
        pid,
        label: opts.label.clone(),
        cmd: cmd_text.clone(),
        cwd: cwd.clone(),
        started_ms: now_ms(),
        weight: opts.weight,
        rss_mb: 0,
    };
    write_slots(&root, &held, &info);

    let mut admit = event("admit");
    admit.wait_ms = Some(wait_ms);
    admit.blocked_ms = Some(blocked.clone());
    admit.avail_gb = admit_snap.avail_gb;
    admit.swap_gb = admit_snap.swap_gb;
    admit.cpu_busy = admit_snap.cpu;
    log_event(&root, &admit);

    let fds: Vec<i32> = held.iter().map(|(_, f)| f.as_raw_fd()).collect();
    let mut command = Command::new(&opts.command[0]);
    command.args(&opts.command[1..]).env("AGENT_BOUNCER_JOB", &job);
    unsafe {
        command.pre_exec(move || {
            libc::setsid();
            for fd in &fds {
                let flags = libc::fcntl(*fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                }
            }
            Ok(())
        });
    }
    let run_started = Instant::now();
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("agent-bouncer: cannot run {}: {err}", opts.command[0]);
            let mut e = event("finish");
            e.exit = Some(127);
            e.dur_ms = Some(0);
            log_event(&root, &e);
            for (i, _) in &held {
                let _ = fs::remove_file(slot_info_path(&root, *i));
            }
            return 127;
        }
    };

    let pgid = child.id() as i32;
    let mut peak: u64 = 0;
    let mut last_sample = Instant::now() - Duration::from_millis(1500);
    let mut signalled: Option<Instant> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => std::process::exit(1),
        }
        let sig = SIGNAL.load(Ordering::SeqCst);
        if sig != 0 && signalled.is_none() {
            unsafe {
                libc::kill(-pgid, sig);
            }
            signalled = Some(Instant::now());
        }
        if signalled.is_some_and(|t| t.elapsed() > Duration::from_secs(5)) {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        if last_sample.elapsed() >= Duration::from_secs(2) {
            last_sample = Instant::now();
            if let Some(mb) = sysinfo::group_rss_mb(pgid) {
                peak = peak.max(mb);
                info.rss_mb = mb;
                write_slots(&root, &held, &info);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let leftover = sysinfo::group_rss_mb(pgid).filter(|mb| *mb > 0);
    if opts.reap {
        reap_group(pgid);
    }
    let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    let mut fin = event("finish");
    fin.dur_ms = Some(run_started.elapsed().as_millis() as u64);
    fin.exit = Some(code);
    fin.peak_rss_mb = Some(peak);
    fin.leftover_mb = leftover;
    log_event(&root, &fin);
    if opts.reap || leftover.is_none() {
        for (i, _) in &held {
            let _ = fs::remove_file(slot_info_path(&root, *i));
        }
    }
    code
}
