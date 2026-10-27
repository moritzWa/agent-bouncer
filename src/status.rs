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
    pub avail_gb: Option<f64>,
    pub swap_gb: Option<f64>,
    pub cpu_busy: Option<f64>,
    pub running: Vec<SlotInfo>,
    pub waiting: Vec<WaitInfo>,
}

fn is_locked(path: &Path) -> bool {
    let Ok(file) = File::open(path) else { return false };
    !flock(&file, libc::LOCK_SH | libc::LOCK_NB)
}

pub fn collect(root: &Path, settings: &Settings, with_cpu: bool) -> Report {
    let mut running: Vec<SlotInfo> = Vec::new();
    let mut in_use = 0;
    for i in 0..settings.slots {
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
            if let Some(info) = fs::read(json).ok().and_then(|b| serde_json::from_slice::<WaitInfo>(&b).ok()) {
                waiting.push(info);
            }
        }
    }
    let mem = sysinfo::memory();
    Report {
        slots_total: settings.slots,
        slots_in_use: in_use,
        avail_gb: mem.map(|m| m.avail_gb),
        swap_gb: mem.map(|m| m.swap_used_gb),
        cpu_busy: if with_cpu { sysinfo::cpu_busy_pct() } else { None },
        running,
        waiting,
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
        "slots {}/{} in use   {}\n",
        report.slots_in_use,
        report.slots_total,
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
                "  {:>7}  {:>8}  weight {}  {}  ({})\n",
                fmt_dur(now.saturating_sub(job.started_ms)),
                fmt_mb(job.rss_mb),
                job.weight,
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
