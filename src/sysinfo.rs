use std::process::Command;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct Memory {
    pub avail_gb: f64,
    pub swap_used_gb: f64,
}

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

pub fn memory() -> Option<Memory> {
    if cfg!(target_os = "macos") {
        let vm = run("vm_stat", &[])?;
        let avail_gb = parse_vm_stat(&vm)?;
        let swap_used_gb = run("sysctl", &["-n", "vm.swapusage"])
            .and_then(|s| parse_swapusage(&s))
            .unwrap_or(0.0);
        Some(Memory { avail_gb, swap_used_gb })
    } else {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        parse_meminfo(&info)
    }
}

/// The one-minute load average: runnable and waiting processes, not CPU percent.
pub fn load_average() -> Option<f64> {
    let mut load = [0.0f64; 1];
    let got = unsafe { libc::getloadavg(load.as_mut_ptr(), 1) };
    (got == 1).then_some(load[0])
}

pub fn cpu_busy_pct() -> Option<f64> {
    let a = cpu_ticks()?;
    std::thread::sleep(Duration::from_millis(300));
    let b = cpu_ticks()?;
    let total = b.0.saturating_sub(a.0);
    let idle = b.1.saturating_sub(a.1);
    if total == 0 {
        return None;
    }
    Some(100.0 * (total - idle) as f64 / total as f64)
}

#[cfg(target_os = "macos")]
fn cpu_ticks() -> Option<(u64, u64)> {
    let mut info = libc::host_cpu_load_info { cpu_ticks: [0; libc::CPU_STATE_MAX as usize] };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    #[allow(deprecated)]
    let rc = unsafe {
        libc::host_statistics(
            libc::mach_host_self(),
            libc::HOST_CPU_LOAD_INFO,
            &mut info as *mut _ as *mut libc::integer_t,
            &mut count,
        )
    };
    if rc != 0 {
        return None;
    }
    let ticks = info.cpu_ticks.map(|t| t as u64);
    Some((ticks.iter().sum(), ticks[libc::CPU_STATE_IDLE as usize]))
}

#[cfg(not(target_os = "macos"))]
fn cpu_ticks() -> Option<(u64, u64)> {
    read_proc_stat()
}

pub fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

#[cfg(not(target_os = "macos"))]
fn read_proc_stat() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let nums: Vec<u64> = line.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    if nums.len() < 5 {
        return None;
    }
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    Some((nums.iter().sum(), idle))
}

pub fn parse_vm_stat(text: &str) -> Option<f64> {
    let mut page = 4096.0;
    if let Some(start) = text.find("page size of ") {
        let rest = &text[start + "page size of ".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(v) = digits.parse::<f64>() {
            page = v;
        }
    }
    let mut reclaimable = 0.0;
    let mut inactive = 0.0;
    let mut file_backed = None;
    let mut seen = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let Ok(v) = value.trim().trim_end_matches('.').parse::<f64>() else { continue };
        match key.trim() {
            "Pages free" | "Pages speculative" | "Pages purgeable" => {
                reclaimable += v;
                seen = true;
            }
            "Pages inactive" => inactive = v,
            "File-backed pages" => file_backed = Some(v),
            _ => {}
        }
    }
    // Inactive pages are mostly apps' own memory, which has to be compressed or
    // swapped out to reuse, so only free, purgeable and file cache count as room.
    // Older macOS versions do not report file-backed pages; use inactive there.
    let pages = reclaimable + file_backed.unwrap_or(inactive);
    seen.then(|| pages * page / GB)
}

pub fn parse_swapusage(text: &str) -> Option<f64> {
    let rest = &text[text.find("used = ")? + "used = ".len()..];
    let token = rest.split_whitespace().next()?;
    let (num, unit) = token.split_at(token.len().checked_sub(1)?);
    let value: f64 = num.parse().ok()?;
    Some(match unit {
        "G" => value,
        "M" => value / 1024.0,
        "K" => value / (1024.0 * 1024.0),
        _ => return None,
    })
}

pub fn parse_meminfo(text: &str) -> Option<Memory> {
    let field = |name: &str| -> Option<f64> {
        text.lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse::<f64>()
            .ok()
    };
    let avail_kb = field("MemAvailable:")?;
    let swap_used_kb = field("SwapTotal:").unwrap_or(0.0) - field("SwapFree:").unwrap_or(0.0);
    Some(Memory {
        avail_gb: avail_kb * 1024.0 / GB,
        swap_used_gb: swap_used_kb.max(0.0) * 1024.0 / GB,
    })
}

pub fn group_rss_mb(pgid: i32) -> Option<u64> {
    let out = run("ps", &["-axo", "pgid=,rss="])?;
    let kb: u64 = out
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let group: i32 = it.next()?.parse().ok()?;
            let rss: u64 = it.next()?.parse().ok()?;
            (group == pgid).then_some(rss)
        })
        .sum();
    Some(kb / 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macos_vm_stat() {
        let text = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                               38704.\nPages active:                            920213.\nPages inactive:                          910669.\nPages speculative:                         1000.\nPages purgeable:                          10000.\n";
        let gb = parse_vm_stat(text).unwrap();
        let expected = (38704.0 + 910669.0 + 1000.0 + 10000.0) * 16384.0 / GB;
        assert!((gb - expected).abs() < 1e-9);
    }

    #[test]
    fn file_cache_counts_as_room_but_inactive_memory_does_not() {
        let text = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 100.\nPages inactive: 5000.\nPages speculative: 10.\nPages purgeable: 20.\nFile-backed pages: 300.\n";
        let gb = parse_vm_stat(text).unwrap();
        assert!((gb - 430.0 * 16384.0 / GB).abs() < 1e-9);
    }

    #[test]
    fn parses_swap_usage() {
        let text = "total = 6144.00M  used = 5310.38M  free = 833.62M  (encrypted)";
        assert!((parse_swapusage(text).unwrap() - 5310.38 / 1024.0).abs() < 1e-9);
    }

    #[test]
    fn parses_linux_meminfo() {
        let text = "MemTotal: 100 kB\nMemAvailable: 2097152 kB\nSwapTotal: 1048576 kB\nSwapFree: 524288 kB\n";
        let m = parse_meminfo(text).unwrap();
        assert!((m.avail_gb - 2.0).abs() < 1e-9);
        assert!((m.swap_used_gb - 0.5).abs() < 1e-9);
    }
}
