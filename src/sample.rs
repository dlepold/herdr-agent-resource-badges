// Process sampling: one scan of all processes per interval, CPU as time used since
// the previous scan. The OS-specific readers live in procs_linux.rs and procs_macos.rs.
// Created 2026-10-22 (David Lepold + Claude Opus 5.5). Last update: 2026-10-23 (nice value, tree_pids).
//
// CPU is reported like `top`: 100 % = one fully busy core, so a pane can exceed 100 %.
// RAM is the summed resident memory of the pane's process tree. Shared pages (e.g. the
// same node binary in several MCP servers) are counted once per process, which
// overstates the truth a little but is cheap to read and good enough for a traffic light.

use std::collections::HashMap;
use std::time::Instant;

#[cfg(target_os = "linux")]
#[path = "procs_linux.rs"]
mod os;
#[cfg(target_os = "macos")]
#[path = "procs_macos.rs"]
mod os;

pub use os::{has_terminal, read_proc};

/// One process, normalised across operating systems.
pub struct Proc {
    pub ppid: u32,
    /// User + system CPU time so far, in nanoseconds.
    pub cpu_ns: u64,
    /// Start time in an OS-specific unit; only compared with itself and with
    /// `os::start_marker_now()`, to tell a reused PID from the same process.
    pub start: u64,
    pub rss_bytes: u64,
    /// Scheduling nice value (-20..19); the throttle reads it before changing it.
    pub nice: i32,
    pub name: String,
}

#[derive(Clone, Default)]
pub struct Usage {
    pub cpu_pct: f64,
    pub rss_bytes: u64,
    pub procs: usize,
    pub top_name: String,
}

pub struct Snapshot {
    pub procs: HashMap<u32, Proc>,
    children: HashMap<u32, Vec<u32>>,
    cpu_ns: HashMap<u32, u64>,
    elapsed: f64,
}

pub struct Sampler {
    prev: HashMap<u32, (u64, u64)>,
    prev_at: Option<Instant>,
    prev_marker: u64,
}

impl Sampler {
    pub fn new() -> Self {
        Sampler { prev: HashMap::new(), prev_at: None, prev_marker: 0 }
    }

    pub fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let marker = os::start_marker_now();
        let procs = os::scan();

        let mut children: HashMap<u32, Vec<u32>> = HashMap::with_capacity(procs.len());
        let mut cpu_ns = HashMap::with_capacity(procs.len());
        for (&pid, p) in &procs {
            children.entry(p.ppid).or_default().push(pid);
            let used = match self.prev.get(&pid) {
                // Same process as last time (start time guards against PID reuse).
                Some(&(ns, start)) if start == p.start => p.cpu_ns.saturating_sub(ns),
                // Born after the previous scan: everything it used belongs to this interval.
                _ if self.prev_at.is_some() && p.start >= self.prev_marker => p.cpu_ns,
                // First scan, or a process we missed: no reliable delta yet.
                _ => 0,
            };
            cpu_ns.insert(pid, used);
        }

        let elapsed = self.prev_at.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        self.prev = procs.iter().map(|(&pid, p)| (pid, (p.cpu_ns, p.start))).collect();
        self.prev_at = Some(now);
        self.prev_marker = marker;

        Snapshot { procs, children, cpu_ns, elapsed }
    }
}

impl Snapshot {
    /// Every PID in the process tree below `root`, including `root` itself (first).
    /// Empty when `root` is not in the snapshot.
    pub fn tree_pids(&self, root: u32) -> Vec<u32> {
        let mut out = Vec::new();
        if !self.procs.contains_key(&root) {
            return out;
        }
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if !self.procs.contains_key(&pid) {
                continue;
            }
            if out.len() > self.procs.len() {
                break; // defensive: a malformed parent map can never loop forever
            }
            out.push(pid);
            if let Some(kids) = self.children.get(&pid) {
                // PID 0 is its own parent on macOS; never walk into the root again.
                stack.extend(kids.iter().filter(|&&k| k != pid));
            }
        }
        out
    }

    /// Sums the whole process tree below `root` (normally the pane's shell).
    pub fn tree_usage(&self, root: u32) -> Option<Usage> {
        let pids = self.tree_pids(root);
        if pids.is_empty() {
            return None;
        }
        let mut usage = Usage::default();
        let mut ns = 0u64;
        let mut top = (0u64, 0u64);
        for pid in pids {
            let Some(p) = self.procs.get(&pid) else { continue };
            let used = self.cpu_ns.get(&pid).copied().unwrap_or(0);
            usage.procs += 1;
            usage.rss_bytes += p.rss_bytes;
            ns += used;
            if (used, p.rss_bytes) > top {
                top = (used, p.rss_bytes);
                usage.top_name = p.name.clone();
            }
        }
        usage.cpu_pct = if self.elapsed > 0.0 { ns as f64 / 1e9 / self.elapsed * 100.0 } else { 0.0 };
        Some(usage)
    }

    /// Nice value of one process in this snapshot.
    pub fn nice_of(&self, pid: u32) -> Option<i32> {
        self.procs.get(&pid).map(|p| p.nice)
    }
}
