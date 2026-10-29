use crate::state::*;
use crate::stats;
use std::collections::HashMap;

/// What to expect from a kind of job that has never run here before.
pub const UNKNOWN_MB: u64 = 1500;
pub const UNKNOWN_CORES: f64 = 2.0;
const MIN_MB: u64 = 200;
const MIN_CORES: f64 = 0.5;

/// How much memory and CPU each kind of job has peaked at before.
pub struct History {
    peaks: HashMap<String, Vec<u64>>,
    cores: HashMap<String, Vec<f64>>,
}

impl History {
    pub fn load(root: &std::path::Path) -> Self {
        Self::from_events(&read_recent_events(root))
    }

    pub fn from_events(events: &[Event]) -> Self {
        let mut peaks: HashMap<String, Vec<u64>> = HashMap::new();
        let mut cores: HashMap<String, Vec<f64>> = HashMap::new();
        for event in events.iter().filter(|e| e.ev == "finish") {
            let key = stats::command_key(event);
            if let Some(peak) = event.peak_rss_mb {
                peaks.entry(key.clone()).or_default().push(peak);
            }
            if let Some(used) = event.peak_cores {
                cores.entry(key).or_default().push(used);
            }
        }
        for list in peaks.values_mut() {
            list.sort_unstable();
        }
        for list in cores.values_mut() {
            list.sort_by(|a, b| a.total_cmp(b));
        }
        History { peaks, cores }
    }

    /// The 90th percentile of what this kind of job peaked at, so most runs fit.
    pub fn predict_mb(&self, label: Option<&str>, cmd: &str) -> u64 {
        match self.peaks.get(&stats::key_of(label, cmd)) {
            Some(list) => stats::percentile(list, 0.9).max(MIN_MB),
            None => UNKNOWN_MB,
        }
    }

    pub fn predict_cores(&self, label: Option<&str>, cmd: &str) -> f64 {
        match self.cores.get(&stats::key_of(label, cmd)) {
            Some(list) => percentile_f(list, 0.9).max(MIN_CORES),
            None => UNKNOWN_CORES,
        }
    }

    pub fn knows(&self, label: Option<&str>, cmd: &str) -> bool {
        self.peaks.contains_key(&stats::key_of(label, cmd))
    }

    /// Cores a job usually takes, across everything seen. For guessing how many run at once.
    pub fn typical_cores(&self) -> f64 {
        let mut all: Vec<f64> = self.cores.values().flatten().copied().collect();
        if all.is_empty() {
            return UNKNOWN_CORES;
        }
        all.sort_by(|a, b| a.total_cmp(b));
        percentile_f(&all, 0.5).max(MIN_CORES)
    }
}

pub fn percentile_f(sorted: &[f64], p: f64) -> f64 {
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Until a job has run this long, its estimate is all there is to go on.
const RAMP_MS: u64 = 90_000;

fn past_ramp(job: &SlotInfo, now_ms: u64) -> bool {
    now_ms.saturating_sub(job.started_ms) >= RAMP_MS
}

/// The estimate, trusted less as the job proves it is smaller. After the ramp-up a
/// job is assumed to need at most 1.5x what it has used so far, so a 20-minute job
/// that never passed 60 MB stops holding back 2 GB for jobs that are waiting.
pub fn expected_peak_mb(job: &SlotInfo, now_ms: u64) -> u64 {
    if !past_ramp(job, now_ms) {
        return job.est_peak_mb;
    }
    let seen = job.peak_mb.max(job.rss_mb);
    job.est_peak_mb.min((seen * 3 / 2).max(seen + 300))
}

pub fn expected_cores(job: &SlotInfo, now_ms: u64) -> f64 {
    if !past_ramp(job, now_ms) {
        return job.est_cores;
    }
    job.est_cores.min((job.peak_cores * 1.5).max(0.5))
}

/// Cores the running jobs are expected to use at their busiest.
pub fn committed_cores(running: &[SlotInfo], now_ms: u64) -> f64 {
    running.iter().map(|job| expected_cores(job, now_ms)).sum()
}

/// Whether a job expected to use `predicted` cores fits beside the running ones.
/// With nothing running it always does, so a job wider than the machine still runs.
pub fn cores_fit(max_cores: f64, committed: f64, predicted: f64, others_running: bool) -> bool {
    !others_running || committed + predicted <= max_cores
}

/// Memory the running jobs are still expected to grow into.
pub fn reserved_mb(running: &[SlotInfo], now_ms: u64) -> u64 {
    running
        .iter()
        .map(|job| expected_peak_mb(job, now_ms).saturating_sub(job.rss_mb))
        .sum()
}

/// Whether a job expected to peak at `predicted_mb` can start without taking
/// free RAM below the floor once everything running has reached its peak. With
/// nothing running it only needs the floor, so a big job can never wait forever.
pub fn fits(
    avail_gb: f64,
    min_free_gb: f64,
    reserved_mb: u64,
    predicted_mb: u64,
    others_running: bool,
) -> bool {
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
            est_cores: 0.0,
            peak_mb: 0,
            peak_cores: 0.0,
        }
    }

    #[test]
    fn predicts_from_the_same_kind_of_job() {
        let events: Vec<Event> = (1..=10)
            .map(|n| finished("cargo test --lib", n * 100))
            .collect();
        let history = History::from_events(&events);
        assert_eq!(history.predict_mb(None, "cargo test"), 900);
        assert_eq!(history.predict_mb(None, "pnpm build"), UNKNOWN_MB);
        let tiny = History::from_events(&[finished("echo-ish thing", 0)]);
        assert_eq!(tiny.predict_mb(None, "echo-ish thing"), MIN_MB);
    }

    #[test]
    fn cores_are_a_budget_but_never_block_an_idle_machine() {
        assert!(cores_fit(18.0, 10.0, 8.0, true));
        assert!(!cores_fit(18.0, 10.0, 8.5, true));
        assert!(cores_fit(18.0, 0.0, 40.0, false));
        let mut done = finished("cargo test", 900);
        done.peak_cores = Some(6.0);
        let history = History::from_events(&[done]);
        assert_eq!(history.predict_cores(None, "cargo test"), 6.0);
        assert_eq!(history.predict_cores(None, "other"), UNKNOWN_CORES);
    }

    #[test]
    fn reserves_what_running_jobs_have_yet_to_use() {
        assert_eq!(
            reserved_mb(&[running(4000, 1000), running(500, 900)], 1_000),
            3000
        );
    }

    #[test]
    fn a_long_running_small_job_stops_reserving_its_whole_estimate() {
        let mut job = running(2400, 60);
        job.est_cores = 2.5;
        job.peak_mb = 80;
        job.peak_cores = 0.1;
        assert_eq!(reserved_mb(&[job.clone()], 1_000), 2340);
        assert_eq!(reserved_mb(&[job.clone()], 120_000), 320);
        assert_eq!(committed_cores(&[job.clone()], 1_000), 2.5);
        assert_eq!(committed_cores(&[job], 120_000), 0.5);
    }

    #[test]
    fn a_job_that_really_grew_keeps_its_reservation() {
        let mut job = running(2400, 2000);
        job.peak_mb = 2200;
        assert_eq!(expected_peak_mb(&job, 120_000), 2400);
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
