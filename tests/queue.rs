use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn bouncer(dir: &Path, slots: usize) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bouncer"));
    cmd.env("AGENT_BOUNCER_DIR", dir)
        .env("AGENT_BOUNCER_SLOTS", slots.to_string())
        .env("AGENT_BOUNCER_MIN_FREE_GB", "0")
        .env("AGENT_BOUNCER_MAX_CPU", "100")
        .env("AGENT_BOUNCER_SETTLE_MS", "0")
        .env("AGENT_BOUNCER_MAX_LOAD", "0")
        .env("AGENT_BOUNCER_MAX_SWAPOUT", "0")
        .env("AGENT_BOUNCER_MODEL", "off")
        .env_remove("AGENT_BOUNCER_JOB");
    cmd
}

fn run_output(dir: &Path, slots: usize, args: &[&str]) -> Output {
    bouncer(dir, slots).args(args).output().unwrap()
}

fn spawn(dir: &Path, slots: usize, args: &[&str]) -> Child {
    bouncer(dir, slots)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {}", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn passes_through_exit_code_and_logs() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_output(dir.path(), 2, &["run", "--", "sh", "-c", "exit 7"]);
    assert_eq!(out.status.code(), Some(7));
    let log = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
    for ev in ["enqueue", "admit", "finish"] {
        assert!(log.contains(&format!("\"ev\":\"{ev}\"")), "missing {ev} in {log}");
    }
    assert!(log.contains("\"exit\":7"));
}

#[test]
fn exits_75_when_all_slots_are_taken() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let mut holder = spawn(
        dir.path(),
        1,
        &["run", "--", "sh", "-c", &format!("touch {}; sleep 3", marker.display())],
    );
    wait_for(&marker);
    let out = run_output(dir.path(), 1, &["run", "--", "true"]);
    assert_eq!(out.status.code(), Some(75));
    assert!(String::from_utf8_lossy(&out.stderr).contains("the AGENT_BOUNCER_SLOTS cap is reached"));
    holder.kill().ok();
    holder.wait().ok();
}

#[test]
fn waiters_are_admitted_in_arrival_order() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let order = dir.path().join("order.txt");
    let mut holder = spawn(
        dir.path(),
        1,
        &["run", "--", "sh", "-c", &format!("touch {}; sleep 2", marker.display())],
    );
    wait_for(&marker);
    let append = |name: &str| format!("echo {name} >> {}", order.display());
    let mut first = spawn(dir.path(), 1, &["run", "--wait", "--", "sh", "-c", &append("first")]);
    std::thread::sleep(Duration::from_millis(300));
    let mut second = spawn(dir.path(), 1, &["run", "--wait", "--", "sh", "-c", &append("second")]);
    std::thread::sleep(Duration::from_millis(300));
    let mut third = spawn(dir.path(), 1, &["run", "--wait", "--", "sh", "-c", &append("third")]);
    for child in [&mut first, &mut second, &mut third, &mut holder] {
        assert!(child.wait().unwrap().success());
    }
    let text = std::fs::read_to_string(&order).unwrap();
    assert_eq!(text.lines().collect::<Vec<_>>(), ["first", "second", "third"]);
}

#[test]
fn weight_takes_several_slots() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let mut heavy = spawn(
        dir.path(),
        2,
        &["run", "--weight", "2", "--", "sh", "-c", &format!("touch {}; sleep 3", marker.display())],
    );
    wait_for(&marker);
    let out = run_output(dir.path(), 2, &["run", "--", "true"]);
    assert_eq!(out.status.code(), Some(75));
    heavy.kill().ok();
    heavy.wait().ok();
}

#[test]
fn rejects_weight_above_slot_count() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_output(dir.path(), 2, &["run", "--weight", "3", "--", "true"]);
    assert_eq!(out.status.code(), Some(64));
}

#[test]
fn max_wait_gives_up_with_75() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let mut holder = spawn(
        dir.path(),
        1,
        &["run", "--", "sh", "-c", &format!("touch {}; sleep 4", marker.display())],
    );
    wait_for(&marker);
    let started = Instant::now();
    let out = run_output(dir.path(), 1, &["run", "--max-wait", "1", "--", "true"]);
    assert_eq!(out.status.code(), Some(75));
    assert!(started.elapsed() < Duration::from_secs(3));
    holder.kill().ok();
    holder.wait().ok();
}

#[test]
fn nested_calls_run_directly_instead_of_deadlocking() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_agent-bouncer");
    let inner = format!("{bin} run --wait -- sh -c 'exit 3'");
    let out = run_output(dir.path(), 1, &["run", "--", "sh", "-c", &inner]);
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn leftover_daemons_are_reaped_when_the_job_ends() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("daemon.pid");
    let script = format!("sleep 60 & echo $! > {}", pidfile.display());
    let out = run_output(dir.path(), 1, &["run", "--", "sh", "-c", &script]);
    assert_eq!(out.status.code(), Some(0));
    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "daemon {pid} survived");
}

#[test]
fn status_and_stats_run() {
    let dir = tempfile::tempdir().unwrap();
    run_output(dir.path(), 2, &["run", "--label", "demo", "--", "true"]);
    let status = run_output(dir.path(), 2, &["status"]);
    assert!(status.status.success());
    let stats = run_output(dir.path(), 2, &["stats", "--since", "1h"]);
    assert!(stats.status.success());
    assert!(String::from_utf8_lossy(&stats.stdout).contains("1"));
}
