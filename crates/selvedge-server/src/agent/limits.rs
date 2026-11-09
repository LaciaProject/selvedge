//! Per-account CPU, memory and process limits for account processes.
//!
//! Where the agent's cgroup (v2) is writable (bare metal, a delegated systemd
//! unit, a privileged pod), each account's workerd goes into its own cgroup
//! with `memory.max`, `cpu.max` and `pids.max`, and the kernel enforces them.
//! Kubernetes mounts a container's cgroup tree read-only, so there the agent
//! enforces them itself, every two seconds:
//!
//! - memory: a process over its limit is killed; the next request starts a
//!   fresh one (as Cloudflare resets an isolate that exceeds its memory);
//! - CPU: a process using more than its share over ten seconds is niced to
//!   10, so under contention other accounts get the CPU first, and back to 0
//!   once it has stayed under for thirty seconds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use selvedge_core::AccountId;

#[derive(Clone, Debug)]
pub struct Limits {
    pub memory_bytes: u64,
    /// CPUs an account may use, e.g. 1.0.
    pub cpus: f64,
    pub pids: u64,
}

pub enum Mode {
    /// Account cgroups are created under this directory.
    Cgroup(PathBuf),
    Watchdog,
}

const CONTROLLERS: &str = "+cpu +memory +pids";
const NICE_THROTTLED: i32 = 10;
const WINDOW: Duration = Duration::from_secs(10);
const RECOVER_AFTER: Duration = Duration::from_secs(30);

/// Sets up account cgroups if this process's cgroup can delegate, else the
/// watchdog.
pub fn detect() -> Mode {
    match setup_cgroups() {
        Ok(dir) => {
            tracing::info!(dir = %dir.display(), "per-account limits: cgroup v2");
            Mode::Cgroup(dir)
        }
        Err(e) => {
            tracing::info!(reason = %e, "per-account limits: enforced by the agent (cgroup tree not writable)");
            Mode::Watchdog
        }
    }
}

fn setup_cgroups() -> std::io::Result<PathBuf> {
    let own = std::fs::read_to_string("/proc/self/cgroup")?;
    let path = own
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| std::io::Error::other("not on cgroup v2"))?;
    let base = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    // cgroup v2 lets a cgroup hand controllers to its children only while it
    // has no processes of its own: move the agent into a child first.
    let agent = base.join("selvedge-agent");
    std::fs::create_dir_all(&agent)?;
    std::fs::write(agent.join("cgroup.procs"), std::process::id().to_string())?;
    std::fs::write(base.join("cgroup.subtree_control"), CONTROLLERS)?;
    let accounts = base.join("selvedge-accounts");
    std::fs::create_dir_all(&accounts)?;
    std::fs::write(accounts.join("cgroup.subtree_control"), CONTROLLERS)?;
    Ok(accounts)
}

/// Puts an account's process into its cgroup, with the limits.
pub fn place(dir: &Path, account: &AccountId, pid: u32, limits: &Limits) -> std::io::Result<()> {
    let cg = dir.join(account.as_str());
    std::fs::create_dir_all(&cg)?;
    std::fs::write(cg.join("memory.max"), limits.memory_bytes.to_string())?;
    let period = 100_000u64;
    std::fs::write(
        cg.join("cpu.max"),
        format!("{} {period}", (limits.cpus * period as f64).round().max(1000.0) as u64),
    )?;
    std::fs::write(cg.join("pids.max"), limits.pids.to_string())?;
    std::fs::write(cg.join("cgroup.procs"), pid.to_string())
}

/// What the watchdog remembers about one process.
struct Seen {
    pid: u32,
    samples: Vec<(Instant, u64)>,
    throttled: bool,
    under_since: Option<Instant>,
}

#[derive(Default)]
pub struct Watchdog {
    seen: HashMap<AccountId, Seen>,
}

/// What the watchdog wants done to a process.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Kill,
}

/// `/proc` reports CPU time in USER_HZ, which is 100 on Linux.
const TICKS_PER_SEC: f64 = 100.0;

/// CPU time (ticks) and resident memory (bytes) of a process.
fn usage(pid: u32) -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name, which may contain spaces.
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15, i.e. 11 and 12 after the name.
    let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some((ticks, kb << 10))
}

/// Sets the nice value of every thread of a process.
fn renice(pid: u32, nice: i32) {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { return };
    for t in tasks.flatten() {
        let tid = t
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
            .and_then(rustix::process::Pid::from_raw);
        if let Some(tid) = tid
            && let Err(e) = rustix::process::setpriority_process(Some(tid), nice)
        {
            tracing::debug!(pid, ?tid, nice, error = %e, "renice failed");
        }
    }
}

impl Watchdog {
    /// Checks one account's process. `processes` are the live ones; accounts
    /// not listed are forgotten.
    pub fn check(&mut self, processes: &[(AccountId, u32)], limits: &Limits) -> Vec<(AccountId, Action)> {
        let now = Instant::now();
        self.seen.retain(|a, s| processes.iter().any(|(b, pid)| a == b && *pid == s.pid));
        let mut actions = Vec::new();
        for (account, pid) in processes {
            let Some((ticks, rss)) = usage(*pid) else { continue };
            if rss > limits.memory_bytes {
                tracing::warn!(%account, pid, rss_mb = rss >> 20, limit_mb = limits.memory_bytes >> 20, "account process exceeded its memory limit; restarting it");
                self.seen.remove(account);
                actions.push((account.clone(), Action::Kill));
                continue;
            }
            let s = self.seen.entry(account.clone()).or_insert(Seen {
                pid: *pid,
                samples: Vec::new(),
                throttled: false,
                under_since: None,
            });
            s.samples.push((now, ticks));
            s.samples.retain(|(t, _)| now.duration_since(*t) <= WINDOW);
            let (t0, k0) = s.samples[0];
            let span = now.duration_since(t0).as_secs_f64();
            if span < WINDOW.as_secs_f64() / 2.0 {
                continue;
            }
            let cpus = (ticks - k0) as f64 / TICKS_PER_SEC / span;
            if cpus > limits.cpus {
                s.under_since = None;
                if !s.throttled {
                    tracing::warn!(%account, pid, cpus = format!("{cpus:.2}"), limit = limits.cpus, "account process is over its CPU share; lowering its priority");
                    renice(*pid, NICE_THROTTLED);
                    crate::metrics::AGENT_CPU_THROTTLED.inc(&[]);
                    s.throttled = true;
                }
            } else if s.throttled {
                let since = *s.under_since.get_or_insert(now);
                if now.duration_since(since) >= RECOVER_AFTER {
                    // Lowering niceness needs CAP_SYS_NICE; without it the
                    // process stays niced until it restarts.
                    renice(*pid, 0);
                    s.throttled = false;
                    s.under_since = None;
                }
            }
            actions.push((account.clone(), Action::None));
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_usage() {
        let (ticks, rss) = usage(std::process::id()).unwrap();
        assert!(rss > 1 << 20);
        let _ = ticks;
    }

    #[test]
    fn kills_over_memory() {
        let mut w = Watchdog::default();
        let a = AccountId::random();
        let tiny = Limits { memory_bytes: 1024, cpus: 1.0, pids: 10 };
        let acts = w.check(&[(a.clone(), std::process::id())], &tiny);
        assert_eq!(acts, vec![(a, Action::Kill)]);
    }
}
