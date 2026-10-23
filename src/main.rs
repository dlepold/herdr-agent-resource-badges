// agent-resource-badges — traffic-light CPU/RAM badges per agent in the herdr sidebar.
// Created 2026-10-22 (David Lepold + Claude Opus 5.5). Last update: 2026-10-22.
//
// The sampler reads /proc once per interval, maps each herdr pane to the process tree
// below its shell (`pane.process_info` -> shell_pid) and reports a short text such as
// "108% 2.0G" as pane metadata. Only panes above the "show" thresholds get a badge,
// under one of three tokens ($res_ok, $res_warn, $res_hot) that the user colours in
// herdr's own config. Every report carries a TTL, so badges vanish by themselves if
// the sampler stops.
//
// Subcommands:
//   ensure            start the sampler for this herdr session unless one runs (idempotent)
//   run               the sampler itself, in the foreground (normally started by `ensure`)
//   report [--once]   live table of all panes (popup pane); --once prints one table
//   status            is a sampler running for this session?
//   enable | disable  switch the badges on or off (all sessions)
//   sidebar-config    print the [ui.sidebar.agents] snippet for herdr's config.toml
//   selftest          check process sampling on this OS without herdr (used by CI)

mod config;
mod herdr;
mod sample;

use config::{Config, Level, PLUGIN_ID};
use herdr::Herdr;
use sample::Sampler;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const LOG_MAX_BYTES: u64 = 512 * 1024;
const LOOKUP_RETRY: Duration = Duration::from_secs(30);

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    match args.first().map(String::as_str) {
        Some("run") => run_daemon(),
        Some("ensure") => ensure(),
        Some("report") => report(flag("--once")),
        Some("status") => status(),
        Some("enable") => enable(),
        Some("disable") => disable(),
        Some("selftest") => selftest(),
        Some("sidebar-config") => {
            print!("{}", sidebar_snippet());
            ExitCode::SUCCESS
        }
        Some("--version") | Some("-V") => {
            println!("{PLUGIN_ID} {VERSION}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!(
                "usage: {PLUGIN_ID} <ensure|run|report [--once]|status|enable|disable|sidebar-config>"
            );
            ExitCode::from(2)
        }
    }
}

// ---------------------------------------------------------------------------
// Paths and the per-session lock
// ---------------------------------------------------------------------------

struct Paths {
    state: PathBuf,
    lock: PathBuf,
    log: PathBuf,
    disabled: PathBuf,
}

impl Paths {
    fn new(herdr: &Herdr) -> Self {
        let state = config::state_dir();
        // One sampler per herdr session: named sessions use different sockets.
        let key: String = herdr
            .socket
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let key = &key[key.len().saturating_sub(80)..];
        Paths {
            lock: state.join(format!("sampler-{key}.lock")),
            log: state.join("sampler.log"),
            disabled: state.join("disabled"),
            state,
        }
    }
}

enum LockState {
    Acquired(File),
    HeldByOther,
}

fn try_lock(paths: &Paths) -> io::Result<LockState> {
    fs::create_dir_all(&paths.state)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&paths.lock)?;
    // An OS file lock instead of a bare PID file: the kernel releases it the moment
    // the holder dies, so a crashed sampler can never block its successor.
    match file.try_lock() {
        Ok(()) => Ok(LockState::Acquired(file)),
        Err(TryLockError::WouldBlock) => Ok(LockState::HeldByOther),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

fn log(paths: &Paths, msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&paths.log) {
        let _ = writeln!(f, "{} [{}] {msg}", unix_secs(), std::process::id());
    }
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// ensure / status / enable / disable
// ---------------------------------------------------------------------------

/// Called by the startup hook and on every `workspace.focused`. herdr runs startup
/// hooks once and does not supervise them, so this re-check is what brings the
/// sampler back after a crash, a reinstall or a plugin install into a running server.
fn ensure() -> ExitCode {
    let herdr = Herdr::from_env();
    let paths = Paths::new(&herdr);
    if paths.disabled.exists() {
        return ExitCode::SUCCESS;
    }
    match try_lock(&paths) {
        Ok(LockState::HeldByOther) => return ExitCode::SUCCESS,
        Ok(LockState::Acquired(file)) => drop(file),
        Err(e) => {
            eprintln!("{PLUGIN_ID}: cannot use {}: {e}", paths.lock.display());
            return ExitCode::FAILURE;
        }
    }
    trim_log(&paths);
    let exe = match env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{PLUGIN_ID}: cannot locate own binary: {e}");
            return ExitCode::FAILURE;
        }
    };
    let out = OpenOptions::new().create(true).append(true).open(&paths.log);
    let (stdout, stderr) = match out.and_then(|f| Ok((f.try_clone()?, f))) {
        Ok((a, b)) => (Stdio::from(a), Stdio::from(b)),
        Err(_) => (Stdio::null(), Stdio::null()),
    };
    // Own process group and no inherited pipes: the hook can exit at once and
    // herdr does not wait for the sampler's output.
    match Command::new(exe)
        .arg("run")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
    {
        Ok(child) => {
            println!("{PLUGIN_ID}: sampler started (pid {})", child.id());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{PLUGIN_ID}: cannot start sampler: {e}");
            ExitCode::FAILURE
        }
    }
}

fn trim_log(paths: &Paths) {
    if fs::metadata(&paths.log).map(|m| m.len() > LOG_MAX_BYTES).unwrap_or(false) {
        let _ = fs::write(&paths.log, b"");
    }
}

fn status() -> ExitCode {
    let herdr = Herdr::from_env();
    let paths = Paths::new(&herdr);
    let running = match try_lock(&paths) {
        Ok(LockState::HeldByOther) => {
            let pid = fs::read_to_string(&paths.lock).unwrap_or_default();
            format!("running (pid {})", pid.trim())
        }
        Ok(LockState::Acquired(_)) => "not running".to_string(),
        Err(e) => format!("unknown ({e})"),
    };
    println!("{PLUGIN_ID} {VERSION}");
    println!("  sampler   {running}");
    println!("  badges    {}", if paths.disabled.exists() { "disabled" } else { "enabled" });
    println!("  socket    {}", herdr.socket.display());
    println!("  config    {}", config::config_path().display());
    println!("  log       {}", paths.log.display());
    ExitCode::SUCCESS
}

fn enable() -> ExitCode {
    let paths = Paths::new(&Herdr::from_env());
    let _ = fs::remove_file(&paths.disabled);
    println!("{PLUGIN_ID}: badges enabled");
    ensure()
}

fn disable() -> ExitCode {
    let paths = Paths::new(&Herdr::from_env());
    if let Err(e) = fs::create_dir_all(&paths.state).and_then(|_| fs::write(&paths.disabled, b"1"))
    {
        eprintln!("{PLUGIN_ID}: cannot write {}: {e}", paths.disabled.display());
        return ExitCode::FAILURE;
    }
    println!("{PLUGIN_ID}: badges disabled; running samplers clear their badges within one interval");
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// The sampler daemon
// ---------------------------------------------------------------------------

struct PaneState {
    terminal_id: String,
    shell_pid: Option<u32>,
    next_lookup: Instant,
    cpu_ema: Option<f64>,
    shown: Level,
    last_text: String,
    last_sent: Option<Instant>,
}

fn run_daemon() -> ExitCode {
    let mut herdr = Herdr::from_env();
    let paths = Paths::new(&herdr);
    let lock = match try_lock(&paths) {
        Ok(LockState::Acquired(file)) => file,
        Ok(LockState::HeldByOther) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{PLUGIN_ID}: lock failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _ = lock.set_len(0);
    let _ = (&lock).write_all(std::process::id().to_string().as_bytes());
    log(&paths, &format!("sampler {VERSION} started for {}", herdr.socket.display()));

    let mut cfg = config::load();
    let mut cfg_mtime = config::modified();
    let exe_id = exe_identity();
    let mut sampler = Sampler::new();
    let mut panes: HashMap<String, PaneState> = HashMap::new();
    let mut failures = 0u32;
    sampler.sample(); // primes the CPU counters; the first real delta follows

    loop {
        thread::sleep(Duration::from_secs_f64(cfg.interval_seconds));

        if paths.disabled.exists() {
            clear_all(&mut herdr, &mut panes);
            log(&paths, "disabled; badges cleared, exiting");
            break;
        }
        if binary_replaced(&exe_id) {
            // A reinstall or rebuild unlinked our executable. Exit so the next
            // `ensure` starts the new build; the TTL clears our badges meanwhile.
            log(&paths, "binary replaced, exiting");
            break;
        }
        let mtime = config::modified();
        if mtime != cfg_mtime {
            cfg = config::load();
            cfg_mtime = mtime;
            log(&paths, &format!("config reloaded: {cfg:?}"));
        }

        let list = match herdr.panes() {
            Ok(list) => {
                failures = 0;
                list
            }
            Err(e) => {
                failures += 1;
                log(&paths, &format!("herdr unreachable ({failures}/3): {e}"));
                if failures >= 3 {
                    // The server is gone; never outlive it.
                    break;
                }
                continue;
            }
        };

        let now = Instant::now();
        let mut seen = HashSet::with_capacity(list.len());
        for p in &list {
            let Some(id) = p.get("pane_id").and_then(Value::as_str) else { continue };
            let terminal = p.get("terminal_id").and_then(Value::as_str).unwrap_or("").to_string();
            seen.insert(id.to_string());
            let st = panes.entry(id.to_string()).or_insert_with(|| PaneState {
                terminal_id: terminal.clone(),
                shell_pid: None,
                next_lookup: now,
                cpu_ema: None,
                shown: Level::Hidden,
                last_text: String::new(),
                last_sent: None,
            });
            if st.terminal_id != terminal {
                // Same pane id, new terminal: the shell was restarted.
                st.terminal_id = terminal;
                st.shell_pid = None;
                st.next_lookup = now;
                st.cpu_ema = None;
            }
            if st.shell_pid.is_none() && now >= st.next_lookup {
                st.shell_pid = herdr.shell_pid(id).ok().flatten().filter(|&pid| is_local_shell(pid));
                st.next_lookup = now + LOOKUP_RETRY;
            }
        }
        panes.retain(|id, _| seen.contains(id));

        let snap = sampler.sample();
        let ttl = cfg.ttl_ms();
        for (id, st) in panes.iter_mut() {
            let Some(pid) = st.shell_pid else { continue };
            let Some(usage) = snap.tree_usage(pid) else {
                st.shell_pid = None; // shell gone; look it up again next round
                continue;
            };
            // Light smoothing so a single spike does not flash the badge.
            let cpu = st.cpu_ema.map_or(usage.cpu_pct, |prev| prev * 0.4 + usage.cpu_pct * 0.6);
            st.cpu_ema = Some(cpu);
            let (cpu_level, ram_level) = cfg.levels(cpu, usage.rss_bytes);
            let level = cpu_level.max(ram_level);

            if level == Level::Hidden {
                if st.shown != Level::Hidden && send(&mut herdr, id, Level::Hidden, "", None) {
                    st.shown = Level::Hidden;
                    st.last_text.clear();
                }
                continue;
            }
            let text = badge_text(cpu, usage.rss_bytes, cfg.badge_both, cpu_level >= ram_level);
            let due = st.last_sent.is_none_or(|t| t.elapsed().as_millis() as u64 >= ttl / 2);
            if (level != st.shown || text != st.last_text || due)
                && send(&mut herdr, id, level, &text, Some(ttl))
            {
                st.shown = level;
                st.last_text = text;
                st.last_sent = Some(Instant::now());
            }
        }
    }
    drop(lock);
    ExitCode::SUCCESS
}

fn send(herdr: &mut Herdr, pane_id: &str, level: Level, text: &str, ttl: Option<u64>) -> bool {
    let mut tokens = Map::new();
    for l in [Level::Ok, Level::Warn, Level::Hot] {
        let value = if l == level { Value::from(text) } else { Value::Null };
        tokens.insert(l.token().unwrap().to_string(), value);
    }
    herdr.report(pane_id, tokens, ttl).is_ok()
}

fn clear_all(herdr: &mut Herdr, panes: &mut HashMap<String, PaneState>) {
    for (id, st) in panes.iter_mut() {
        if st.shown != Level::Hidden {
            send(herdr, id, Level::Hidden, "", None);
            st.shown = Level::Hidden;
        }
    }
}

/// A pane on a remote herdr machine reports a PID from that machine, which would
/// hit an unrelated local process. Locally, a pane's shell is a child of the herdr
/// server; after a live handoff it may be reparented, so a shell with a terminal
/// is accepted as well.
fn is_local_shell(pid: u32) -> bool {
    let parent_is_herdr = sample::read_proc(pid)
        .and_then(|p| sample::read_proc(p.ppid))
        .is_some_and(|parent| parent.name.starts_with("herdr"));
    parent_is_herdr || sample::has_terminal(pid)
}

/// Path and inode of our executable at start. A rebuild or reinstall gives the path
/// a new inode (or removes it); the sampler then exits and `ensure` starts the new
/// build. Works the same on Linux and macOS.
fn exe_identity() -> Option<(PathBuf, u64)> {
    let path = env::current_exe().ok()?;
    let ino = fs::metadata(&path).ok()?.ino();
    Some((path, ino))
}

fn binary_replaced(start: &Option<(PathBuf, u64)>) -> bool {
    match start {
        Some((path, ino)) => fs::metadata(path).map(|m| m.ino() != *ino).unwrap_or(true),
        None => false,
    }
}

/// herdr gives every token in a row an equal share of the sidebar width, so a
/// narrow sidebar leaves only 3-5 columns per token. By default the badge names
/// just the metric that set its colour: "108%" or "2.8G".
fn badge_text(cpu: f64, rss: u64, both: bool, cpu_dominates: bool) -> String {
    match (both, cpu_dominates) {
        (true, _) => format!("{:.0}% {}", cpu, mem_text(rss)),
        (false, true) => format!("{:.0}%", cpu),
        (false, false) => mem_text(rss),
    }
}

fn mem_text(rss: u64) -> String {
    let mib = rss as f64 / 1_048_576.0;
    if mib >= 1024.0 {
        format!("{:.1}G", mib / 1024.0)
    } else {
        format!("{mib:.0}M")
    }
}

// ---------------------------------------------------------------------------
// Self-test: exercises the OS process reader without herdr (CI on Linux and macOS)
// ---------------------------------------------------------------------------

fn selftest() -> ExitCode {
    let me = std::process::id();
    let mut ok = true;
    let mut check = |name: &str, pass: bool, detail: String| {
        println!("{} {name}: {detail}", if pass { "PASS" } else { "FAIL" });
        ok &= pass;
    };

    // A child that keeps one core busy, so the tree below us has measurable CPU.
    let mut child = match Command::new("sh").args(["-c", "while :; do :; done"]).spawn() {
        Ok(c) => c,
        Err(e) => {
            println!("FAIL spawn busy child: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut sampler = Sampler::new();
    sampler.sample();
    thread::sleep(Duration::from_millis(1500));
    let t = Instant::now();
    let snap = sampler.sample();
    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
    let kid = child.id();
    let _ = child.kill();
    let _ = child.wait();

    check("scan", snap.procs.len() >= 5, format!("{} processes in {scan_ms:.1} ms", snap.procs.len()));
    let own = sample::read_proc(me);
    check(
        "own process",
        own.as_ref().is_some_and(|p| p.rss_bytes > 512 * 1024 && !p.name.is_empty()),
        own.as_ref()
            .map(|p| format!("name {:?}, RSS {}, ppid {}", p.name, mem_text(p.rss_bytes), p.ppid))
            .unwrap_or_else(|| "not readable".into()),
    );
    let kid_proc = snap.procs.get(&kid);
    check(
        "child's parent",
        kid_proc.is_some_and(|p| p.ppid == me),
        kid_proc.map(|p| format!("ppid {} (expected {me}), name {:?}", p.ppid, p.name)).unwrap_or_else(|| "child not found".into()),
    );
    match snap.tree_usage(me) {
        Some(u) => {
            check("tree size", u.procs >= 2, format!("{} processes below and including us", u.procs));
            check("tree CPU", u.cpu_pct > 30.0, format!("{:.0}% (a busy child should give ~100%)", u.cpu_pct));
            check("tree RAM", u.rss_bytes > 512 * 1024, mem_text(u.rss_bytes));
        }
        None => check("tree", false, "own PID missing from snapshot".into()),
    }
    if ok {
        println!("selftest passed on {} {}", env::consts::OS, env::consts::ARCH);
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// Report pane
// ---------------------------------------------------------------------------

struct Row {
    level: Level,
    cpu: f64,
    rss: u64,
    procs: usize,
    agent: String,
    title: String,
    space: String,
    top: String,
}

fn report(once: bool) -> ExitCode {
    let mut herdr = Herdr::from_env();
    let cfg = config::load();
    let mut sampler = Sampler::new();
    let mut shells: HashMap<String, Option<u32>> = HashMap::new();
    sampler.sample();
    thread::sleep(Duration::from_secs(1));

    let quit = if once {
        None
    } else {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let _ = io::stdin().read_line(&mut line);
            let _ = tx.send(());
        });
        Some(rx)
    };

    loop {
        let list = match herdr.panes() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{PLUGIN_ID}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let spaces: HashMap<String, String> = herdr.workspace_labels().into_iter().collect();
        let snap = sampler.sample();
        let mut rows = Vec::new();
        for p in &list {
            let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let id = s("pane_id");
            let pid = *shells.entry(id.clone()).or_insert_with(|| {
                herdr.shell_pid(&id).ok().flatten().filter(|&pid| is_local_shell(pid))
            });
            let Some(usage) = pid.and_then(|pid| snap.tree_usage(pid)) else { continue };
            rows.push(Row {
                level: cfg.level(usage.cpu_pct, usage.rss_bytes),
                cpu: usage.cpu_pct,
                rss: usage.rss_bytes,
                procs: usage.procs,
                agent: s("agent"),
                title: s("terminal_title_stripped"),
                space: spaces.get(&s("workspace_id")).cloned().unwrap_or_default(),
                top: usage.top_name,
            });
        }
        rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.rss.cmp(&a.rss)));
        print!("{}", render(&rows, snap.procs.len(), &cfg, once));
        let _ = io::stdout().flush();

        match &quit {
            None => return ExitCode::SUCCESS,
            Some(rx) => {
                if rx.recv_timeout(Duration::from_secs(2)).is_ok() {
                    return ExitCode::SUCCESS;
                }
            }
        }
    }
}

fn render(rows: &[Row], total_procs: usize, cfg: &Config, once: bool) -> String {
    const RESET: &str = "\x1b[0m";
    const DIM: &str = "\x1b[2m";
    const BOLD: &str = "\x1b[1m";
    let colour = |l: Level| match l {
        Level::Hot => "\x1b[38;2;243;139;168m",
        Level::Warn => "\x1b[38;2;250;179;135m",
        Level::Ok => "\x1b[38;2;166;227;161m",
        Level::Hidden => DIM,
    };
    let cut = |s: &str, n: usize| -> String {
        if s.chars().count() <= n {
            s.to_string()
        } else {
            s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
        }
    };
    let total_rss: u64 = rows.iter().map(|r| r.rss).sum();
    let total_cpu: f64 = rows.iter().map(|r| r.cpu).sum();
    let busy = rows.iter().filter(|r| r.level != Level::Hidden).count();

    let mut out = String::new();
    if !once {
        out.push_str("\x1b[H\x1b[2J");
    }
    out.push_str(&format!(
        "{BOLD}Agent resources{RESET}  {} panes · {busy} above threshold · {total_procs} processes · panes total {:.0}% CPU, {}\n\n",
        rows.len(),
        total_cpu,
        mem_text(total_rss)
    ));
    out.push_str(&format!(
        "{DIM}   {:>6} {:>7} {:>5}  {:<7} {:<16} {:<40} {}{RESET}\n",
        "CPU", "RAM", "PROCS", "AGENT", "SPACE", "PANE", "BUSIEST"
    ));
    for r in rows {
        let c = colour(r.level);
        let dot = if r.level == Level::Hidden { "·" } else { "●" };
        out.push_str(&format!(
            "{c}{dot}{RESET}  {c}{:>5.0}% {:>7}{RESET} {:>5}  {:<7} {:<16} {:<40} {DIM}{}{RESET}\n",
            r.cpu,
            mem_text(r.rss),
            r.procs,
            cut(if r.agent.is_empty() { "-" } else { &r.agent }, 7),
            cut(&r.space, 16),
            cut(&r.title, 40),
            cut(&r.top, 18),
        ));
    }
    out.push_str(&format!(
        "\n{DIM}CPU: 100 % = one core · badge from {:.0}/{:.0}/{:.0} % CPU or {}/{}/{} RAM · config {}{RESET}\n",
        cfg.cpu_show,
        cfg.cpu_warn,
        cfg.cpu_hot,
        mem_text((cfg.ram_show_mb * 1_048_576.0) as u64),
        mem_text((cfg.ram_warn_mb * 1_048_576.0) as u64),
        mem_text((cfg.ram_hot_mb * 1_048_576.0) as u64),
        config::config_path().display()
    ));
    out.push_str(&format!(
        "{DIM}{PLUGIN_ID} {VERSION} · made by the team behind tutoring-agency.com{}{RESET}\n",
        if once { "" } else { " · Enter closes" }
    ));
    out
}

fn sidebar_snippet() -> &'static str {
    r##"# agent-resource-badges: put the three badge tokens first in a row. Merge them
# into your existing [ui.sidebar.agents] rows rather than replacing your layout.
#
# herdr gives every token in a row an equal share of the width. In a crowded row
# a badge can shrink to "10…"; hiding a value that is the same for most agents
# leaves it room, e.g.  { token = "agent", rules = [{ equals = "claude", hide = true }] }
[ui.sidebar.agents]
rows = [
  ["state_icon", "machine", "workspace", "tab"],
  [
    { token = "$res_hot",  fg = "#f38ba8", bold = true },
    { token = "$res_warn", fg = "#fab387" },
    { token = "$res_ok",   fg = "#a6e3a1" },
    "agent",
  ],
]
"##
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_is_short() {
        assert_eq!(badge_text(108.4, 2147483648, true, true), "108% 2.0G");
        assert_eq!(badge_text(108.4, 1000 << 20, false, true), "108%");
        assert_eq!(badge_text(3.0, 2867 << 20, false, false), "2.8G");
    }
}
