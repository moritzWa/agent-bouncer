use crate::run::{slot_info_path, slot_lock_path};
use crate::state::*;
use crate::sysinfo;
use serde::Serialize;
use std::fs::{self, File};
use std::path::Path;

#[derive(Serialize, Debug)]
pub struct Report {
    pub slots_total: usize,
    pub slots_in_use: usize,
    pub cores_committed: f64,
    pub cores_total: f64,
    pub avail_gb: Option<f64>,
    pub swap_gb: Option<f64>,
    pub cpu_busy: Option<f64>,
    pub running: Vec<SlotInfo>,
    pub waiting: Vec<WaitInfo>,
    /// Milliseconds until the queue is empty, guessed from how long the same
    /// kinds of job took before, or how long jobs usually take for a kind it
    /// has not seen. None only when nothing has ever finished.
    pub est_free_ms: Option<u64>,
}

/// When the last job should finish, if every job takes as long as it usually
/// does: running jobs get what is left of their usual time (a job past it is
/// assumed to need about as long again), and each waiting job starts where the
/// first running job ends.
pub fn estimate_free_ms(
    root: &Path,
    _slots: usize,
    running: &[SlotInfo],
    waiting: &[WaitInfo],
    now: u64,
) -> Option<u64> {
    if running.is_empty() && waiting.is_empty() {
        return None;
    }
    let mut durations: std::collections::HashMap<String, Vec<u64>> = Default::default();
    for event in read_recent_events(root) {
        if let (true, Some(dur)) = (event.ev == "finish", event.dur_ms) {
            durations
                .entry(crate::stats::command_key(&event))
                .or_default()
                .push(dur);
        }
    }
    let mut everything: Vec<u64> = durations.values().flatten().copied().collect();
    everything.sort_unstable();
    if everything.is_empty() {
        return None;
    }
    let history = |label: Option<&str>, cmd: &str| -> Vec<u64> {
        match durations.get(&crate::stats::key_of(label, cmd)) {
            Some(list) => {
                let mut list = list.clone();
                list.sort_unstable();
                list
            }
            None => everything.clone(),
        }
    };
    // A job already past its usual time is not done. It is somewhere in the
    // long tail, so it has about as long left as it has already run.
    let left_of = |job: &SlotInfo| -> u64 {
        let list = history(job.label.as_deref(), &job.cmd);
        let elapsed = now.saturating_sub(job.started_ms);
        let usual = crate::stats::percentile(&list, 0.5);
        let slow = crate::stats::percentile(&list, 0.9);
        if elapsed < usual {
            usual - elapsed
        } else if elapsed < slow {
            slow - elapsed
        } else {
            elapsed / 2
        }
    };
    // A job only waits when the machine is full, so it takes over the place of
    // whichever running job ends first, however many slots are configured.
    let mut free_at: Vec<u64> = running.iter().map(left_of).collect();
    if free_at.is_empty() {
        free_at.push(0);
    }
    for job in waiting {
        let duration = crate::stats::percentile(&history(job.label.as_deref(), &job.cmd), 0.5);
        let slot = free_at.iter_mut().min()?;
        *slot += duration;
    }
    free_at.into_iter().max()
}

fn is_locked(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    !flock(&file, libc::LOCK_SH | libc::LOCK_NB)
}

/// The jobs holding slots right now, and how many slots are held.
pub fn running_jobs(root: &Path, _slots: usize) -> (Vec<SlotInfo>, usize) {
    let mut running: Vec<SlotInfo> = Vec::new();
    let mut in_use = 0;
    let existing = fs::read_dir(root.join("slots"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.strip_prefix("slot-")?
                .strip_suffix(".lock")?
                .parse::<usize>()
                .ok()
        })
        .max()
        .map_or(0, |top| top + 1);
    for i in 0..existing {
        if !is_locked(&slot_lock_path(root, i)) {
            continue;
        }
        in_use += 1;
        let info = fs::read(slot_info_path(root, i))
            .ok()
            .and_then(|b| serde_json::from_slice::<SlotInfo>(&b).ok());
        if let Some(info) = info {
            if !running.iter().any(|r| r.job == info.job) {
                running.push(info);
            }
        }
    }
    (running, in_use)
}

pub fn collect(root: &Path, settings: &Settings, with_cpu: bool) -> Report {
    let (running, in_use) = running_jobs(root, settings.slots);
    let mut waiting: Vec<WaitInfo> = Vec::new();
    if let Ok(entries) = fs::read_dir(root.join("waiting")) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".lock"))
            .collect();
        names.sort();
        for name in names {
            let dir = root.join("waiting");
            if !is_locked(&dir.join(&name)) {
                continue;
            }
            let json = dir.join(name.replace(".lock", ".json"));
            if let Some(info) = fs::read(json)
                .ok()
                .and_then(|b| serde_json::from_slice::<WaitInfo>(&b).ok())
            {
                waiting.push(info);
            }
        }
    }
    let mem = sysinfo::memory();
    let per_job = crate::headroom::History::load(root).typical_cores();
    let concurrency = ((settings.max_cores / per_job).round() as usize)
        .max(running.len())
        .clamp(1, settings.slots);
    let est_free_ms = estimate_free_ms(root, concurrency, &running, &waiting, now_ms());
    Report {
        slots_total: if settings.slots == UNCAPPED_SLOTS {
            0
        } else {
            settings.slots
        },
        slots_in_use: in_use,
        cores_committed: crate::headroom::committed_cores(&running),
        cores_total: settings.max_cores,
        avail_gb: mem.map(|m| m.avail_gb),
        swap_gb: mem.map(|m| m.swap_used_gb),
        cpu_busy: if with_cpu {
            sysinfo::cpu_busy_pct()
        } else {
            None
        },
        running,
        waiting,
        est_free_ms,
    }
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let mut machine = Vec::new();
    if let Some(v) = report.avail_gb {
        machine.push(format!("free RAM {v:.1} GB"));
    }
    if let Some(v) = report.swap_gb {
        machine.push(format!("swap {v:.1} GB"));
    }
    if let Some(v) = report.cpu_busy {
        machine.push(format!("CPU {v:.0}%"));
    }
    out.push_str(&format!(
        "{} running   cores {:.0}/{:.0} expected   {}\n",
        report.running.len(),
        report.cores_committed,
        report.cores_total,
        machine.join("   ")
    ));
    let now = now_ms();
    if report.running.is_empty() && report.slots_in_use == 0 {
        out.push_str("\nnothing running\n");
    }
    if !report.running.is_empty() {
        out.push_str("\nrunning\n");
        for job in &report.running {
            let name = job.label.clone().unwrap_or_else(|| shorten(&job.cmd, 48));
            out.push_str(&format!(
                "  {:>7}  {:>8} of ~{:<8} {:>4.1} cores  {}  ({})\n",
                fmt_dur(now.saturating_sub(job.started_ms)),
                fmt_mb(job.rss_mb),
                fmt_mb(job.est_peak_mb),
                job.est_cores,
                name,
                shorten(&job.cwd, 40)
            ));
        }
    }
    if !report.waiting.is_empty() {
        out.push_str("\nwaiting\n");
        for (n, job) in report.waiting.iter().enumerate() {
            let name = job.label.clone().unwrap_or_else(|| shorten(&job.cmd, 48));
            out.push_str(&format!(
                "  {}. {:>7}  {:<5}  {}  ({})\n",
                n + 1,
                fmt_dur(now.saturating_sub(job.since_ms)),
                job.reason,
                name,
                shorten(&job.cwd, 40)
            ));
        }
    }
    out
}

#[cfg(test)]
mod estimate_tests {
    use super::*;

    fn slot(label: &str, started_ms: u64) -> SlotInfo {
        SlotInfo {
            job: label.into(),
            pid: 1,
            label: Some(label.into()),
            cmd: "x".into(),
            cwd: "/".into(),
            started_ms,
            weight: 1,
            rss_mb: 0,
            est_peak_mb: 0,
            est_cores: 0.0,
        }
    }

    fn wait(label: &str) -> WaitInfo {
        WaitInfo {
            job: label.into(),
            pid: 2,
            label: Some(label.into()),
            cmd: "x".into(),
            cwd: "/".into(),
            since_ms: 0,
            weight: 1,
            reason: "slots".into(),
        }
    }

    fn history(root: &Path, finished: &[(&str, u64)]) {
        let lines: String = finished
            .iter()
            .map(|(label, dur)| {
                format!("{{\"ts\":1,\"ev\":\"finish\",\"job\":\"j\",\"label\":\"{label}\",\"dur_ms\":{dur}}}\n")
            })
            .collect();
        fs::write(root.join("events.jsonl"), lines).unwrap();
    }

    #[test]
    fn waiting_jobs_start_on_the_slot_that_frees_first() {
        let dir = tempfile::tempdir().unwrap();
        history(dir.path(), &[("a", 10_000), ("b", 30_000), ("c", 8_000)]);
        let running = [slot("a", 0), slot("b", 0)];
        let free = estimate_free_ms(dir.path(), 2, &running, &[wait("c")], 4_000);
        // a ends in 6s, c then runs 8s there; b still has 26s.
        assert_eq!(free, Some(26_000));
        let free = estimate_free_ms(
            dir.path(),
            2,
            &running,
            &[wait("c"), wait("c"), wait("c")],
            4_000,
        );
        assert_eq!(free, Some(30_000));
    }

    #[test]
    fn unknown_jobs_take_the_typical_time_and_no_history_means_no_estimate() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            estimate_free_ms(dir.path(), 2, &[slot("zzz", 0)], &[], 1),
            None
        );
        history(dir.path(), &[("a", 10_000)]);
        assert_eq!(
            estimate_free_ms(dir.path(), 2, &[slot("zzz", 0)], &[], 1_000),
            Some(9_000)
        );
        assert_eq!(estimate_free_ms(dir.path(), 2, &[], &[], 1), None);
    }

    #[test]
    fn a_job_past_its_usual_time_is_not_done() {
        let dir = tempfile::tempdir().unwrap();
        history(dir.path(), &[("a", 10_000), ("a", 12_000), ("a", 14_000)]);
        let long = [slot("a", 0)];
        assert_eq!(
            estimate_free_ms(dir.path(), 2, &long, &[], 600_000),
            Some(300_000)
        );
        assert!(estimate_free_ms(dir.path(), 2, &long, &[], 13_000).unwrap() > 0);
    }

    #[test]
    fn waiting_jobs_queue_behind_running_ones_even_with_no_slot_cap() {
        let dir = tempfile::tempdir().unwrap();
        history(dir.path(), &[("a", 10_000), ("c", 8_000)]);
        let free = estimate_free_ms(dir.path(), 1_000_000, &[slot("a", 0)], &[wait("c")], 4_000);
        assert_eq!(free, Some(14_000));
    }
}
