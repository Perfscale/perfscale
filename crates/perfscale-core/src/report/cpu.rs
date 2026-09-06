//! Host CPU utilization for the shipper's adaptive gate.
//!
//! Reads aggregate jiffies from `/proc/stat` and diffs successive calls, so
//! [`CpuReader::sample`] is the system-wide busy percentage over the period
//! between calls. Both the CLI and the agent share this helper. Every failure
//! mode — non-Linux host, unreadable file, first call (no previous sample),
//! counter wrap — yields `None`, which callers must treat as "gate inert".

use std::sync::Mutex;

/// Diff-based CPU reader: stateful (keeps the previous jiffies), cheap
/// (one file read per sample), safe to share behind an `Arc`.
#[derive(Debug, Default)]
pub struct CpuReader {
    prev: Mutex<Option<Jiffies>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Jiffies {
    busy: u64,
    total: u64,
}

impl CpuReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// System-wide busy CPU% (0–100) since the previous call. `None` on the
    /// first call, off-Linux, or on any read/parse failure.
    pub fn sample(&self) -> Option<f64> {
        let cur = read_proc_stat()?;
        let prev = self.prev.lock().unwrap().replace(cur)?;
        let total = cur.total.checked_sub(prev.total)?;
        let busy = cur.busy.checked_sub(prev.busy)?;
        if total == 0 {
            return None;
        }
        Some(busy as f64 / total as f64 * 100.0)
    }
}

fn read_proc_stat() -> Option<Jiffies> {
    let content = std::fs::read_to_string("/proc/stat").ok()?;
    parse_proc_stat(&content)
}

/// Parse the aggregate `cpu` line: busy = total − (idle + iowait), matching
/// the usual `/proc/stat` utilization formula.
fn parse_proc_stat(content: &str) -> Option<Jiffies> {
    let line = content.lines().next()?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let nums: Vec<u64> = fields.filter_map(|f| f.parse().ok()).collect();
    // user nice system idle iowait [irq softirq steal guest guest_nice]
    if nums.len() < 4 {
        return None;
    }
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    let total: u64 = nums.iter().sum();
    Some(Jiffies {
        busy: total.checked_sub(idle)?,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aggregate_cpu_line() {
        let content = "cpu  100 0 50 800 25 0 5 0 0 0\ncpu0 60 0 30 380 10 0 2 0 0 0\n";
        let j = parse_proc_stat(content).unwrap();
        // total = 980, idle = 800 + 25 (iowait counts as idle).
        assert_eq!(
            j,
            Jiffies {
                busy: 155,
                total: 980
            }
        );
    }

    #[test]
    fn rejects_garbage_and_short_lines() {
        assert!(parse_proc_stat("").is_none());
        assert!(parse_proc_stat("notcpu 1 2 3 4\n").is_none());
        assert!(parse_proc_stat("cpu  1 2 3\n").is_none());
    }

    #[test]
    fn sample_is_none_without_two_readings_on_linux() {
        // Off-Linux /proc/stat is missing entirely, so every call is None;
        // on Linux the FIRST call has no previous sample to diff against.
        let reader = CpuReader::new();
        assert!(reader.sample().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn second_sample_is_a_percentage_on_linux() {
        let reader = CpuReader::new();
        assert!(reader.sample().is_none());
        std::thread::sleep(std::time::Duration::from_millis(5));
        let pct = reader
            .sample()
            .expect("second read diffs against the first");
        assert!((0.0..=100.0).contains(&pct), "unexpected cpu %: {pct}");
    }
}
