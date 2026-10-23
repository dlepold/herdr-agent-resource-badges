// Linux process reader: /proc/<pid>/stat for every process.
// Created 2026-10-22 (David Lepold + Claude Opus 5.5). Last update: 2026-10-22.

use super::Proc;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::sync::OnceLock;

/// Page size and clock ticks per second, read once from the ELF auxiliary vector.
fn constants() -> (u64, u64) {
    static C: OnceLock<(u64, u64)> = OnceLock::new();
    *C.get_or_init(aux_values)
}

pub fn scan() -> HashMap<u32, Proc> {
    let mut procs = HashMap::with_capacity(1024);
    let mut buf = String::with_capacity(512);
    let Ok(dir) = fs::read_dir("/proc") else { return procs };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if let Some(p) = read_into(pid, &mut buf) {
            procs.insert(pid, p);
        }
    }
    procs
}

pub fn read_proc(pid: u32) -> Option<Proc> {
    read_into(pid, &mut String::with_capacity(512))
}

fn read_into(pid: u32, buf: &mut String) -> Option<Proc> {
    buf.clear();
    fs::File::open(format!("/proc/{pid}/stat")).ok()?.read_to_string(buf).ok()?;
    let (page, tck) = constants();
    let s = parse_stat(buf)?;
    Some(Proc {
        ppid: s.ppid,
        cpu_ns: s.ticks.saturating_mul(1_000_000_000) / tck,
        start: s.start,
        rss_bytes: s.rss_pages * page,
        name: s.name,
    })
}

/// Start times are clock ticks since boot, so "now" is the uptime in ticks.
pub fn start_marker_now() -> u64 {
    let (_, tck) = constants();
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()))
        .map(|secs| (secs * tck as f64) as u64)
        .unwrap_or(0)
}

/// True when the process's stdin is one of our pseudo-terminals.
pub fn has_terminal(pid: u32) -> bool {
    fs::read_link(format!("/proc/{pid}/fd/0")).is_ok_and(|t| t.to_string_lossy().starts_with("/dev/pts/"))
}

struct Stat {
    ppid: u32,
    ticks: u64,
    start: u64,
    rss_pages: u64,
    name: String,
}

/// Parses /proc/<pid>/stat. The command name sits in parentheses and may itself
/// contain spaces or ')', so everything is split at the LAST ')'.
fn parse_stat(s: &str) -> Option<Stat> {
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
    Some(Stat {
        ppid: f[1].parse().ok()?,
        ticks: f[11].parse::<u64>().ok()? + f[12].parse::<u64>().ok()?,
        start: f[19].parse().ok()?,
        rss_pages: f[21].parse::<i64>().ok()?.max(0) as u64,
        name,
    })
}

/// Page size and clock ticks per second from /proc/self/auxv, so no libc binding is
/// needed. Falls back to the common Linux defaults.
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
