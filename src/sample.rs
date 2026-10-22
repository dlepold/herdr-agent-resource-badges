// Process sampling from /proc: one scan per interval, CPU as ticks since the previous scan.
// Created 2026-10-22 (David Lepold + Claude Opus 5.5). Last update: 2026-10-22.
//
// CPU is reported like `top`: 100 % = one fully busy core, so a pane can exceed 100 %.
// RAM is the summed RSS of the pane's process tree. Shared pages (e.g. the same node
// binary in several MCP servers) are counted once per process, which overstates the
// truth a little but is cheap to read and good enough for a traffic light.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::time::Instant;

pub struct Proc {
    pub ppid: u32,
    pub ticks: u64,
    pub start: u64,
    pub rss_pages: u64,
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
    cpu_ticks: HashMap<u32, u64>,
    elapsed: f64,
    clk_tck: u64,
    page_size: u64,
}

pub struct Sampler {
    prev: HashMap<u32, (u64, u64)>,
    prev_at: Option<Instant>,
    prev_uptime_ticks: u64,
    clk_tck: u64,
    page_size: u64,
    buf: String,
}

impl Sampler {
    pub fn new() -> Self {
        let (page_size, clk_tck) = aux_values();
        Sampler {
            prev: HashMap::new(),
            prev_at: None,
            prev_uptime_ticks: 0,
            clk_tck,
            page_size,
            buf: String::with_capacity(512),
        }
    }

    pub fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let uptime_ticks = read_uptime_ticks(self.clk_tck);
        let mut procs = HashMap::with_capacity(self.prev.len() + 64);
        if let Ok(dir) = fs::read_dir("/proc") {
            for entry in dir.flatten() {
                let name = entry.file_name();
                let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                    continue;
                };
                if let Some(proc_) = self.read_stat(pid) {
                    procs.insert(pid, proc_);
                }
            }
        }

        let mut children: HashMap<u32, Vec<u32>> = HashMap::with_capacity(procs.len());
        let mut cpu_ticks = HashMap::with_capacity(procs.len());
        for (&pid, p) in &procs {
            children.entry(p.ppid).or_default().push(pid);
            let used = match self.prev.get(&pid) {
                // Same process as last time (start time guards against PID reuse).
                Some(&(ticks, start)) if start == p.start => p.ticks.saturating_sub(ticks),
                // Born after the previous scan: everything it used belongs to this interval.
                _ if self.prev_at.is_some() && p.start >= self.prev_uptime_ticks => p.ticks,
                // First scan, or a process we missed: no reliable delta yet.
                _ => 0,
            };
            cpu_ticks.insert(pid, used);
        }

        let elapsed = self
            .prev_at
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        self.prev = procs.iter().map(|(&pid, p)| (pid, (p.ticks, p.start))).collect();
        self.prev_at = Some(now);
        self.prev_uptime_ticks = uptime_ticks;

        Snapshot {
            procs,
            children,
            cpu_ticks,
            elapsed,
            clk_tck: self.clk_tck,
            page_size: self.page_size,
        }
    }

    fn read_stat(&mut self, pid: u32) -> Option<Proc> {
        self.buf.clear();
        let mut file = fs::File::open(format!("/proc/{pid}/stat")).ok()?;
        file.read_to_string(&mut self.buf).ok()?;
        parse_stat(&self.buf)
    }
}

impl Snapshot {
    /// Sums the whole process tree below `root` (normally the pane's shell).
    pub fn tree_usage(&self, root: u32) -> Option<Usage> {
        if !self.procs.contains_key(&root) {
            return None;
        }
        let mut usage = Usage::default();
        let mut ticks = 0u64;
        let mut top = (0u64, 0u64);
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            let Some(p) = self.procs.get(&pid) else { continue };
            if usage.procs > self.procs.len() {
                break; // defensive: a malformed parent map can never loop forever
            }
            let used = self.cpu_ticks.get(&pid).copied().unwrap_or(0);
            usage.procs += 1;
            usage.rss_bytes += p.rss_pages * self.page_size;
            ticks += used;
            if (used, p.rss_pages) > top {
                top = (used, p.rss_pages);
                usage.top_name = p.name.clone();
            }
            if let Some(kids) = self.children.get(&pid) {
                stack.extend(kids);
            }
        }
        usage.cpu_pct = if self.elapsed > 0.0 {
            ticks as f64 / self.clk_tck as f64 / self.elapsed * 100.0
        } else {
            0.0
        };
        Some(usage)
    }
}

/// Parses /proc/<pid>/stat. The command name sits in parentheses and may itself
/// contain spaces or ')', so everything is split at the LAST ')'.
pub fn parse_stat(s: &str) -> Option<Proc> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let name = s.get(open + 1..close)?.to_string();
    // Fields after ')': 0 state, 1 ppid, 11 utime, 12 stime, 19 starttime, 21 rss (pages).
    let mut f = [""; 22];
    let mut n = 0;
    for (i, v) in s.get(close + 1..)?.split_ascii_whitespace().take(22).enumerate() {
        f[i] = v;
        n = i + 1;
    }
    if n < 22 {
        return None;
    }
    Some(Proc {
        ppid: f[1].parse().ok()?,
        ticks: f[11].parse::<u64>().ok()? + f[12].parse::<u64>().ok()?,
        start: f[19].parse().ok()?,
        rss_pages: f[21].parse::<i64>().ok()?.max(0) as u64,
        name,
    })
}

/// Page size and clock ticks per second from the ELF auxiliary vector,
/// so no libc binding is needed. Falls back to the common Linux defaults.
fn aux_values() -> (u64, u64) {
    const AT_PAGESZ: usize = 6;
    const AT_CLKTCK: usize = 17;
    let (mut page, mut tck) = (4096u64, 100u64);
    if let Ok(bytes) = fs::read("/proc/self/auxv") {
        let w = std::mem::size_of::<usize>();
        for pair in bytes.chunks_exact(2 * w) {
            let key = usize::from_ne_bytes(pair[..w].try_into().unwrap());
            let val = usize::from_ne_bytes(pair[w..].try_into().unwrap()) as u64;
            match key {
                AT_PAGESZ if val > 0 => page = val,
                AT_CLKTCK if val > 0 => tck = val,
                _ => {}
            }
        }
    }
    (page, tck)
}

fn read_uptime_ticks(clk_tck: u64) -> u64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()))
        .map(|secs| (secs * clk_tck as f64) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_with_spaces_and_parens() {
        let line = "4242 (tmux: server (x)) S 1 4242 4242 0 -1 4194560 100 0 0 0 \
                    25 7 0 0 20 0 1 0 99887 12345678 2048 18446744073709551615";
        let p = parse_stat(line).expect("parse");
        assert_eq!(p.name, "tmux: server (x)");
        assert_eq!(p.ppid, 1);
        assert_eq!(p.ticks, 32);
        assert_eq!(p.start, 99887);
        assert_eq!(p.rss_pages, 2048);
    }

    #[test]
    fn rejects_truncated_lines() {
        assert!(parse_stat("1 (init) S 0 1").is_none());
    }
}
