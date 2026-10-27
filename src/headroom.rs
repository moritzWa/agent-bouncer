use crate::state::*;
use crate::stats;
use std::collections::HashMap;

/// What to expect from a kind of job that has never run here before.
const UNKNOWN_MB: u64 = 1500;
const MIN_MB: u64 = 200;

/// How much memory each kind of job has peaked at before.
pub struct History {
    peaks: HashMap<String, Vec<u64>>,
}

impl History {
    pub fn load(root: &std::path::Path) -> Self {
        Self::from_events(&read_recent_events(root))
    }

    pub fn from_events(events: &[Event]) -> Self {
        let mut peaks: HashMap<String, Vec<u64>> = HashMap::new();
        for event in events.iter().filter(|e| e.ev == "finish") {
            if let Some(peak) = event.peak_rss_mb {
                peaks.entry(stats::command_key(event)).or_default().push(peak);
            }
        }
        for list in peaks.values_mut() {
            list.sort_unstable();
        }
        History { peaks }
    }

    /// The 90th percentile of what this kind of job peaked at, so most runs fit.
    pub fn predict_mb(&self, label: Option<&str>, cmd: &str) -> u64 {
        match self.peaks.get(&stats::key_of(label, cmd)) {
            Some(list) => stats::percentile(list, 0.9).max(MIN_MB),
            None => UNKNOWN_MB,
        }
    }
}

/// Memory the running jobs are still expected to grow into.
pub fn reserved_mb(running: &[SlotInfo]) -> u64 {
    running.iter().map(|job| job.est_peak_mb.saturating_sub(job.rss_mb)).sum()
}

/// Whether a job expected to peak at `predicted_mb` can start without taking
/// free RAM below the floor once everything running has reached its peak. With
/// nothing running it only needs the floor, so a big job can never wait forever.
pub fn fits(avail_gb: f64, min_free_gb: f64, reserved_mb: u64, predicted_mb: u64, others_running: bool) -> bool {
    if !others_running {
        return avail_gb >= min_free_gb;
    }
    avail_gb - (reserved_mb + predicted_mb) as f64 / 1024.0 >= min_free_gb
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished(cmd: &str, peak: u64) -> Event {
        Event {
            ev: "finish".into(),
            cmd: Some(cmd.into()),
            peak_rss_mb: Some(peak),
            ..Default::default()
        }
    }

    fn running(est: u64, rss: u64) -> SlotInfo {
        SlotInfo {
            job: "j".into(),
            pid: 1,
            label: None,
            cmd: "x".into(),
            cwd: "/".into(),
            started_ms: 0,
            weight: 1,
            rss_mb: rss,
            est_peak_mb: est,
        }
    }

    #[test]
    fn predicts_from_the_same_kind_of_job() {
        let events: Vec<Event> = (1..=10).map(|n| finished("cargo test --lib", n * 100)).collect();
        let history = History::from_events(&events);
        assert_eq!(history.predict_mb(None, "cargo test"), 900);
        assert_eq!(history.predict_mb(None, "pnpm build"), UNKNOWN_MB);
        let tiny = History::from_events(&[finished("echo-ish thing", 0)]);
        assert_eq!(tiny.predict_mb(None, "echo-ish thing"), MIN_MB);
    }

    #[test]
    fn reserves_what_running_jobs_have_yet_to_use() {
        assert_eq!(reserved_mb(&[running(4000, 1000), running(500, 900)]), 3000);
    }

    #[test]
    fn admission_counts_jobs_that_have_not_peaked_yet() {
        // 16 GB free, one job at 0.1 GB that will reach 6 GB: a 5 GB job still fits above a 3 GB floor.
        assert!(fits(16.0, 3.0, 5900, 5000, true));
        // With 9 GB free it does not.
        assert!(!fits(9.0, 3.0, 5900, 5000, true));
        // Nothing running: only the floor matters, however big the job is.
        assert!(fits(5.0, 3.0, 0, 20_000, false));
        assert!(!fits(2.0, 3.0, 0, 100, false));
    }
}
