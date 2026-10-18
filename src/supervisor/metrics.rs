//! Background RAM/CPU sampling from /proc, so API calls return a cached value
//! instantly instead of blocking on a CPU-delta measurement.

use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

/// USER_HZ, the unit of utime/stime in /proc/[pid]/stat.
static CLOCK_TICKS: LazyLock<f64> = LazyLock::new(|| {
    // SAFETY: sysconf has no preconditions.
    match unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) } {
        ticks if ticks > 0 => ticks as f64,
        _ => 100.0,
    }
});

static PROC_AVAILABLE: LazyLock<bool> = LazyLock::new(|| std::path::Path::new("/proc/self/stat").exists());

#[derive(Debug, Default, Clone, Copy)]
pub struct Metrics {
    pub rss_bytes: u64,
    /// Not normalized per core (like top/ps), so it can exceed 100.
    pub cpu_percent: f64,
}

/// Samples one pid until dropped.
pub struct MetricsSampler {
    latest: Arc<Mutex<Metrics>>,
    task: JoinHandle<()>,
}

impl MetricsSampler {
    pub fn spawn(pid: u32, interval: Duration) -> Self {
        let latest = Arc::new(Mutex::new(Metrics::default()));
        let shared = Arc::clone(&latest);
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            let mut last: Option<(u64, Instant)> = None;
            let mut warned = false;
            loop {
                // The first tick fires immediately, so RSS is available right away.
                ticker.tick().await;
                let sample = match (read_cpu_ticks(pid), read_rss_bytes(pid)) {
                    (Ok(ticks), Ok(rss)) => {
                        let now = Instant::now();
                        let cpu_percent = match last {
                            Some((last_ticks, last_time)) => {
                                let elapsed = now.duration_since(last_time).as_secs_f64();
                                let used = ticks.saturating_sub(last_ticks) as f64 / *CLOCK_TICKS;
                                if elapsed > 0.0 { used / elapsed * 100.0 } else { 0.0 }
                            }
                            None => 0.0,
                        };
                        last = Some((ticks, now));
                        Metrics { rss_bytes: rss, cpu_percent }
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        // With /proc present, a failed read just means the
                        // process is gone or a zombie - not worth a warning.
                        if !warned && !*PROC_AVAILABLE {
                            warned = true;
                            eprintln!("[metrics] /proc unavailable for pid {pid}, reporting zero values: {e}");
                        }
                        Metrics::default()
                    }
                };
                *shared.lock().unwrap_or_else(PoisonError::into_inner) = sample;
            }
        });
        Self { latest, task }
    }

    pub fn snapshot(&self) -> Metrics {
        *self.latest.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for MetricsSampler {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// utime+stime (fields 14 and 15 per proc(5)). The comm field is
/// parenthesized and may contain spaces, so parsing starts after the last ')'.
fn read_cpu_ticks(pid: u32) -> Result<u64, String> {
    let path = format!("/proc/{pid}/stat");
    let data = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let after_comm = data.rfind(')').map(|i| &data[i + 1..]).ok_or("unexpected stat format")?;
    // after_comm starts at field 3 (state), so field N is at index N-3.
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let field = |n: usize| -> Result<u64, String> {
        fields
            .get(n - 3)
            .ok_or("unexpected stat field count")?
            .parse()
            .map_err(|e| format!("{path}: {e}"))
    };
    Ok(field(14)? + field(15)?)
}

fn read_rss_bytes(pid: u32) -> Result<u64, String> {
    let path = format!("/proc/{pid}/status");
    let data = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let line = data
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .ok_or_else(|| format!("VmRSS not found in {path}"))?;
    let kb: u64 = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("unexpected VmRSS format in {path}"))?
        .parse()
        .map_err(|e| format!("{path}: {e}"))?;
    Ok(kb * 1024)
}
