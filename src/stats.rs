use crate::state::*;
use std::collections::BTreeMap;

#[derive(Default, Debug, Clone)]
pub struct Day {
    pub jobs: usize,
    pub wait_ms: u64,
    pub max_wait_ms: u64,
    pub timeouts: usize,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub finished: usize,
    pub run_ms: u64,
    pub admitted: usize,
    pub waits: Vec<u64>,
    pub waited_jobs: usize,
    pub blocked: BTreeMap<String, u64>,
    pub timeouts: usize,
    pub by_day: BTreeMap<String, Day>,
    pub peaks: BTreeMap<String, (usize, u64)>,
    pub leftover_jobs: usize,
    pub leftover_mb: u64,
}

const WAITED_MS: u64 = 1000;

pub fn command_key(event: &Event) -> String {
    key_of(event.label.as_deref(), event.cmd.as_deref().unwrap_or_default())
}

/// Jobs with the same label, or the same first two words of the command, are
/// treated as the same kind of job.
pub fn key_of(label: Option<&str>, cmd: &str) -> String {
    if let Some(label) = label {
        return label.to_string();
    }
    let words: Vec<&str> = cmd.split_whitespace().take(2).collect();
    words
        .iter()
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn aggregate(events: &[Event], since_ms: u64, day_of: &dyn Fn(u64) -> String) -> Stats {
    let mut stats = Stats::default();
    for event in events.iter().filter(|e| e.ts >= since_ms) {
        let day = day_of(event.ts);
        match event.ev.as_str() {
            "admit" => {
                let wait = event.wait_ms.unwrap_or(0);
                stats.admitted += 1;
                stats.waits.push(wait);
                if wait >= WAITED_MS {
                    stats.waited_jobs += 1;
                }
                add_blocked(&mut stats.blocked, event);
                let d = stats.by_day.entry(day).or_default();
                d.jobs += 1;
                d.wait_ms += wait;
                d.max_wait_ms = d.max_wait_ms.max(wait);
            }
            "timeout" => {
                let wait = event.wait_ms.unwrap_or(0);
                stats.timeouts += 1;
                add_blocked(&mut stats.blocked, event);
                let d = stats.by_day.entry(day).or_default();
                d.timeouts += 1;
                d.wait_ms += wait;
                d.max_wait_ms = d.max_wait_ms.max(wait);
            }
            "finish" => {
                stats.finished += 1;
                stats.run_ms += event.dur_ms.unwrap_or(0);
                let peak = event.peak_rss_mb.unwrap_or(0);
                let entry = stats.peaks.entry(command_key(event)).or_default();
                entry.0 += 1;
                entry.1 = entry.1.max(peak);
                if let Some(mb) = event.leftover_mb {
                    stats.leftover_jobs += 1;
                    stats.leftover_mb += mb;
                }
            }
            _ => {}
        }
    }
    stats.waits.sort_unstable();
    stats
}

fn add_blocked(total: &mut BTreeMap<String, u64>, event: &Event) {
    if let Some(map) = &event.blocked_ms {
        for (reason, ms) in map {
            *total.entry(reason.clone()).or_default() += ms;
        }
    }
}

pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

pub fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim();
    let (num, unit) = text.split_at(text.len().checked_sub(1)?);
    let value: u64 = num.parse().ok()?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return None,
    };
    Some(value * secs * 1000)
}

pub fn render(stats: &Stats, window: &str) -> String {
    let mut out = format!("agent-bouncer stats, last {window}\n\n");
    if stats.admitted == 0 && stats.timeouts == 0 {
        out.push_str("no jobs recorded in this window\n");
        return out;
    }
    let pct = (100 * stats.waited_jobs).checked_div(stats.admitted).unwrap_or(0);
    out.push_str(&format!(
        "jobs run      {}   total run time {}\n",
        stats.finished,
        fmt_dur(stats.run_ms)
    ));
    out.push_str(&format!(
        "waited        {} of {} jobs ({}%) waited 1s or more\n",
        stats.waited_jobs, stats.admitted, pct
    ));
    let total_wait: u64 = stats.by_day.values().map(|d| d.wait_ms).sum();
    out.push_str(&format!(
        "wait          total {}   median {}   p90 {}   max {}\n",
        fmt_dur(total_wait),
        fmt_dur(percentile(&stats.waits, 0.5)),
        fmt_dur(percentile(&stats.waits, 0.9)),
        fmt_dur(stats.waits.last().copied().unwrap_or(0))
    ));
    if !stats.blocked.is_empty() {
        let parts: Vec<String> = stats
            .blocked
            .iter()
            .map(|(reason, ms)| format!("{reason} {}", fmt_dur(*ms)))
            .collect();
        out.push_str(&format!("blocked by    {}\n", parts.join("   ")));
    }
    if stats.timeouts > 0 {
        out.push_str(&format!("gave up       {} jobs hit their wait limit\n", stats.timeouts));
    }
    if stats.leftover_jobs > 0 {
        out.push_str(&format!(
            "leftovers     {} jobs left processes behind ({} in total, killed)\n",
            stats.leftover_jobs,
            fmt_mb(stats.leftover_mb)
        ));
    }
    out.push_str("\nby day\n");
    for (day, d) in &stats.by_day {
        out.push_str(&format!(
            "  {day}  jobs {:>3}   wait {:>7}   longest {:>7}   gave up {}\n",
            d.jobs,
            fmt_dur(d.wait_ms),
            fmt_dur(d.max_wait_ms),
            d.timeouts
        ));
    }
    let mut peaks: Vec<_> = stats.peaks.iter().filter(|(_, v)| v.1 > 0).collect();
    peaks.sort_by_key(|a| std::cmp::Reverse(a.1 .1));
    if !peaks.is_empty() {
        out.push_str("\nheaviest commands (peak RAM)\n");
        for (key, (count, mb)) in peaks.into_iter().take(8) {
            out.push_str(&format!("  {:>8}  {}x  {}\n", fmt_mb(*mb), count, key));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str, ts: u64) -> Event {
        Event { ts, ev: name.into(), job: "j".into(), ..Default::default() }
    }

    fn day(ts: u64) -> String {
        format!("d{}", ts / 86_400_000)
    }

    #[test]
    fn aggregates_waits_blocks_and_peaks() {
        let mut a = ev("admit", 1_000);
        a.wait_ms = Some(5_000);
        a.blocked_ms = Some(BTreeMap::from([("slots".to_string(), 4_000), ("ram".to_string(), 1_000)]));
        let mut b = ev("admit", 2_000);
        b.wait_ms = Some(200);
        let mut t = ev("timeout", 3_000);
        t.wait_ms = Some(60_000);
        t.blocked_ms = Some(BTreeMap::from([("slots".to_string(), 60_000)]));
        let mut f = ev("finish", 4_000);
        f.dur_ms = Some(10_000);
        f.peak_rss_mb = Some(2048);
        f.cmd = Some("/usr/bin/cargo test --all".into());
        f.leftover_mb = Some(300);
        let stats = aggregate(&[a, b, t, f], 0, &day);
        assert_eq!(stats.admitted, 2);
        assert_eq!(stats.waited_jobs, 1);
        assert_eq!(stats.timeouts, 1);
        assert_eq!(stats.blocked["slots"], 64_000);
        assert_eq!(stats.blocked["ram"], 1_000);
        assert_eq!(stats.run_ms, 10_000);
        assert_eq!(stats.peaks["cargo test"], (1, 2048));
        assert_eq!(stats.leftover_jobs, 1);
        assert_eq!(stats.by_day["d0"].wait_ms, 65_200);
    }

    #[test]
    fn since_filters_old_events() {
        let mut old = ev("admit", 10);
        old.wait_ms = Some(9_000);
        let mut new = ev("admit", 5_000);
        new.wait_ms = Some(1_000);
        let stats = aggregate(&[old, new], 100, &day);
        assert_eq!(stats.admitted, 1);
        assert_eq!(stats.waits, vec![1_000]);
    }

    #[test]
    fn percentiles_and_durations() {
        assert_eq!(percentile(&[1, 2, 3, 4, 5], 0.5), 3);
        assert_eq!(percentile(&[], 0.9), 0);
        assert_eq!(parse_duration("2h"), Some(7_200_000));
        assert_eq!(parse_duration("7d"), Some(604_800_000));
        assert_eq!(parse_duration("x"), None);
        assert_eq!(parse_duration("m"), None);
    }
}
