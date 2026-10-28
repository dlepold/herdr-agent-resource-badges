// agent-resource-badges — traffic-light CPU/RAM badges per agent in the herdr sidebar.
// Created 2026-10-22 (David Lepold + Claude Opus 5.5). Last update: 2026-10-23 (throttle).
//
// The sampler reads /proc once per interval, maps each herdr pane to the process tree
// below its shell (`pane.process_info` -> shell_pid) and reports a short text such as
// "108% 2.0G" as pane metadata. Only panes above the "show" thresholds get a badge,
// under one of three tokens ($res_ok, $res_warn, $res_hot) that the user colours in
// herdr's own config. Every report carries a TTL, so badges vanish by themselves if
// the sampler stops.
//
// Throttle (optional, `throttle_nice` in config.toml): a pane whose tree is above
// `throttle_cpu` % CPU while the pane has NO focus gets its tree set to that nice
// value, so the agent you are looking at and the rest of the desktop stay responsive.
// The moment the pane is focused the tree goes back to nice 0 — at once through the
// `pane.focused` hook (`unnice`), and at the latest on the next sampling round.
//
// Subcommands:
//   ensure            start the sampler for this herdr session unless one runs (idempotent)
//   run               the sampler itself, in the foreground (normally started by `ensure`)
//   report [--once]   live table of all panes (popup pane); --once prints one table
//   status            is a sampler running for this session?
//   enable | disable  switch the badges on or off (all sessions)
//   sidebar-config    print the [ui.sidebar.agents] snippet for herdr's config.toml
//   selftest          check process sampling on this OS without herdr (used by CI)
//   unnice            hook for `pane.focused`: restore nice 0 for the focused pane's tree
//   setnice N PID…    root helper used through `sudo -n` to lower nice values

mod config;
mod herdr;
mod nice;
mod sample;

use config::{Config, Level, PLUGIN_ID};
use herdr::Herdr;
use nice::Lower;
use sample::{Sampler, Snapshot};
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
        Some("unnice") => unnice(),
        Some("setnice") => nice::setnice_cli(&args[1..]),
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
                "usage: {PLUGIN_ID} <ensure|run|report [--once]|status|enable|disable|sidebar-config|unnice|setnice N PID…>"
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
    let cfg = config::load();
    if cfg.throttle_enabled() {
        println!(
            "  throttle  nice +{} for unfocused panes from {:.0}% CPU · lowering via {}",
            cfg.throttle_nice,
            cfg.throttle_cpu,
            nice::probe_lower().label()
        );
    } else {
        println!("  throttle  off (set throttle_nice = 5 in config.toml to enable)");
    }
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
    /// From `pane.list`; exactly the pane the user is typing in.
    focused: bool,
    /// "agent · title" for log lines.
    label: String,
    /// The tree is currently at the throttle nice value because of us.
    throttled: bool,
    /// Normalisation (negative nice -> 0) is logged once per pane, then done silently:
    /// tools like ananicy may re-apply a negative value to short-lived children every
    /// scan, and a log line per round would be noise.
    normalize_logged: bool,
}

/// What the throttle does with one pane this round. Pure, so it can be tested.
#[derive(Debug, PartialEq, Eq)]
enum Throttle {
    Keep,
    Apply,
    Release,
}

fn throttle_step(enabled: bool, focused: bool, throttled: bool, cpu: f64, from_cpu: f64) -> Throttle {
    match (enabled, focused, throttled) {
        (false, _, true) | (_, true, true) => Throttle::Release,
        (true, false, false) if cpu >= from_cpu => Throttle::Apply,
        _ => Throttle::Keep,
    }
}

/// Raises every process below the pane's shell (not the shell itself, which is idle
/// anyway) that still sits below the throttle value. Runs every round while the pane
/// is throttled, so late-born children and processes some other tool reset are caught
/// again; a few setpriority calls per round cost nothing. Returns how many were raised.
fn throttle_apply(snap: &Snapshot, shell: u32, value: i32) -> usize {
    snap.tree_pids(shell)
        .into_iter()
        .filter(|&p| p != shell)
        .flat_map(|p| nice::matching_tasks(p, |n| n < value))
        .filter(|&p| nice::set(p, value).is_ok())
        .count()
}

/// herdr starts pane shells at a negative nice value (-4 here), which every agent
/// started inside inherits: "High" priority for a background job is the opposite of
/// what the throttle is for. While the throttle is enabled, anything below a pane's
/// shell (the shell included) that runs below nice 0 is brought up to 0. Raising a
/// nice value needs no privileges. Returns how many were changed.
fn normalize_negative(snap: &Snapshot, shell: u32) -> usize {
    snap.tree_pids(shell)
        .into_iter()
        .flat_map(|p| nice::matching_tasks(p, |n| n < 0))
        .filter(|&p| nice::set(p, 0).is_ok())
        .count()
}

/// PIDs below the shell that currently carry the throttle value — the ones to restore.
fn throttled_pids(snap: &Snapshot, shell: u32, value: i32) -> Vec<u32> {
    snap.tree_pids(shell)
        .into_iter()
        .filter(|&p| p != shell)
        .flat_map(|p| nice::matching_tasks(p, |n| n == value))
        .collect()
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

    // How a raised nice value can be lowered again; probed once, when the throttle
    // is (or becomes) enabled. Without a way to lower, the throttle stays off: a tree
    // we could not restore would stay slow for good.
    let mut lower: Option<Lower> = None;
    let mut throttle_on = throttle_ready(&cfg, &mut lower, &paths);

    loop {
        thread::sleep(Duration::from_secs_f64(cfg.interval_seconds));

        if paths.disabled.exists() {
            clear_all(&mut herdr, &mut panes);
            release_all(&mut sampler, &mut panes, &cfg, &lower, &paths, "disabled");
            log(&paths, "disabled; badges cleared, exiting");
            break;
        }
        if binary_replaced(&exe_id) {
            // A reinstall or rebuild unlinked our executable. Exit so the next
            // `ensure` starts the new build; the TTL clears our badges meanwhile.
            release_all(&mut sampler, &mut panes, &cfg, &lower, &paths, "binary replaced");
            log(&paths, "binary replaced, exiting");
            break;
        }
        let mtime = config::modified();
        if mtime != cfg_mtime {
            let old = cfg.clone();
            cfg = config::load();
            cfg_mtime = mtime;
            log(&paths, &format!("config reloaded: {cfg:?}"));
            if old.throttle_nice != cfg.throttle_nice && old.throttle_enabled() {
                // The value changed (or the throttle went off): restore with the OLD value,
                // otherwise those processes would never be found again.
                release_all(&mut sampler, &mut panes, &old, &lower, &paths, "config changed");
            }
            throttle_on = throttle_ready(&cfg, &mut lower, &paths);
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
                    release_all(&mut sampler, &mut panes, &cfg, &lower, &paths, "herdr gone");
                    break;
                }
                continue;
            }
        };

        let now = Instant::now();
        let mut seen = HashSet::with_capacity(list.len());
        for p in &list {
            let Some(id) = p.get("pane_id").and_then(Value::as_str) else { continue };
            let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("");
            let terminal = s("terminal_id").to_string();
            seen.insert(id.to_string());
            let st = panes.entry(id.to_string()).or_insert_with(|| PaneState {
                terminal_id: terminal.clone(),
                shell_pid: None,
                next_lookup: now,
                cpu_ema: None,
                shown: Level::Hidden,
                last_text: String::new(),
                last_sent: None,
                focused: false,
                label: String::new(),
                throttled: false,
                normalize_logged: false,
            });
            st.focused = p.get("focused").and_then(Value::as_bool).unwrap_or(false);
            st.label = format!("{} · {}", s("agent"), s("terminal_title_stripped"));
            if st.terminal_id != terminal {
                // Same pane id, new terminal: the shell was restarted.
                st.terminal_id = terminal;
                st.shell_pid = None;
                st.next_lookup = now;
                st.cpu_ema = None;
                st.throttled = false;
            }
            if st.shell_pid.is_none() && now >= st.next_lookup {
                st.shell_pid = herdr.shell_pid(id).ok().flatten().filter(|&pid| is_local_shell(pid));
                st.next_lookup = now + LOOKUP_RETRY;
            }
        }
        // A closed pane normally takes its processes with it; if some survive, they
        // must not stay throttled with nobody left to restore them.
        let gone: Vec<String> = panes.keys().filter(|id| !seen.contains(*id)).cloned().collect();
        if gone.iter().any(|id| panes[id].throttled) {
            let snap = sampler.sample();
            for id in &gone {
                let st = panes.get_mut(id).unwrap();
                if st.throttled {
                    release_pane(&snap, id, st, &cfg, &lower, &paths, "pane closed");
                }
            }
        }
        panes.retain(|id, _| seen.contains(id));

        let snap = sampler.sample();
        let ttl = cfg.ttl_ms();
        for (id, st) in panes.iter_mut() {
            let Some(pid) = st.shell_pid else { continue };
            let Some(usage) = snap.tree_usage(pid) else {
                st.shell_pid = None; // shell gone; look it up again next round
                st.throttled = false;
                continue;
            };
            // Light smoothing so a single spike does not flash the badge.
            let cpu = st.cpu_ema.map_or(usage.cpu_pct, |prev| prev * 0.4 + usage.cpu_pct * 0.6);
            st.cpu_ema = Some(cpu);
            let (cpu_level, ram_level) = cfg.levels(cpu, usage.rss_bytes);
            let level = cpu_level.max(ram_level);

            if throttle_on {
                let n = normalize_negative(&snap, pid);
                if n > 0 && !st.normalize_logged {
                    log(&paths, &format!("normalize {id} [{}]: nice 0 on {n} threads that ran below 0 (further ones silently)", st.label));
                    st.normalize_logged = true;
                }
            }
            match throttle_step(throttle_on, st.focused, st.throttled, cpu, cfg.throttle_cpu) {
                Throttle::Apply => {
                    let n = throttle_apply(&snap, pid, cfg.throttle_nice);
                    st.throttled = true;
                    log(
                        &paths,
                        &format!(
                            "throttle {id} [{}] {:.0}% CPU, no focus: nice +{} on {n} threads across {} processes",
                            st.label, cpu, cfg.throttle_nice, usage.procs
                        ),
                    );
                }
                Throttle::Release => {
                    let why = if st.focused { "focused" } else { "throttle off" };
                    release_pane(&snap, id, st, &cfg, &lower, &paths, why);
                }
                Throttle::Keep => {
                    if st.throttled {
                        // Children inherit their parent's nice value, so most new
                        // processes arrive throttled; this catches the rest, e.g. after
                        // a focus flip too short for the sampler to notice.
                        throttle_apply(&snap, pid, cfg.throttle_nice);
                    }
                }
            }

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

// ---------------------------------------------------------------------------
// Throttle helpers
// ---------------------------------------------------------------------------

/// True when the throttle is enabled AND a raised nice value can be lowered again.
/// The probe runs once per sampler life; its outcome is logged either way.
fn throttle_ready(cfg: &Config, lower: &mut Option<Lower>, paths: &Paths) -> bool {
    if !cfg.throttle_enabled() {
        return false;
    }
    if lower.is_none() {
        let probe = nice::probe_lower();
        match &probe {
            Lower::Unavailable(why) => log(paths, &format!("throttle OFF: {why}")),
            how => log(
                paths,
                &format!(
                    "throttle ON: nice +{} for unfocused panes from {:.0}% CPU; lowering via {}",
                    cfg.throttle_nice,
                    cfg.throttle_cpu,
                    how.label()
                ),
            ),
        }
        *lower = Some(probe);
    }
    !matches!(lower, Some(Lower::Unavailable(_)))
}

/// Puts one pane's tree back to nice 0 and forgets the throttle state either way.
fn release_pane(
    snap: &Snapshot,
    id: &str,
    st: &mut PaneState,
    cfg: &Config,
    lower: &Option<Lower>,
    paths: &Paths,
    why: &str,
) {
    st.throttled = false;
    let Some(shell) = st.shell_pid else { return };
    let pids = throttled_pids(snap, shell, cfg.throttle_nice);
    if pids.is_empty() {
        return; // already restored, e.g. by the `unnice` hook
    }
    let how = lower.clone().unwrap_or(Lower::Unavailable("not probed".into()));
    match nice::lower(&pids, 0, &how) {
        Ok(n) => log(paths, &format!("release {id} [{}] ({why}): nice 0 on {n} threads via {}", st.label, how.label())),
        Err(e) => log(paths, &format!("release {id} [{}] ({why}) FAILED for {} threads: {e}", st.label, pids.len())),
    }
}

/// Restores every throttled pane; used on every exit path of the sampler.
fn release_all(
    sampler: &mut Sampler,
    panes: &mut HashMap<String, PaneState>,
    cfg: &Config,
    lower: &Option<Lower>,
    paths: &Paths,
    why: &str,
) {
    if !panes.values().any(|st| st.throttled) {
        return;
    }
    let snap = sampler.sample();
    for (id, st) in panes.iter_mut() {
        if st.throttled {
            release_pane(&snap, id, st, cfg, lower, paths, why);
        }
    }
}

/// Hook for herdr's `pane.focused` event: the focused pane's tree goes back to nice 0
/// right away instead of on the next sampling round. herdr passes the pane in
/// HERDR_PANE_ID; without it every focused pane is handled.
fn unnice() -> ExitCode {
    let cfg = config::load();
    if !cfg.throttle_enabled() {
        return ExitCode::SUCCESS;
    }
    let mut herdr = Herdr::from_env();
    let paths = Paths::new(&herdr);
    let ids: Vec<String> = match env::var("HERDR_PANE_ID") {
        Ok(id) if !id.is_empty() => vec![id],
        _ => herdr
            .panes()
            .unwrap_or_default()
            .iter()
            .filter(|p| p.get("focused").and_then(Value::as_bool).unwrap_or(false))
            .filter_map(|p| p.get("pane_id").and_then(Value::as_str).map(str::to_string))
            .collect(),
    };
    let snap = Sampler::new().sample();
    let mut ok = true;
    for id in ids {
        let Some(shell) = herdr.shell_pid(&id).ok().flatten().filter(|&pid| is_local_shell(pid)) else {
            continue;
        };
        let pids = throttled_pids(&snap, shell, cfg.throttle_nice);
        if pids.is_empty() {
            continue;
        }
        match nice::lower_best_effort(&pids, 0) {
            Ok(n) => {
                println!("{PLUGIN_ID}: {id} focused: nice 0 on {n} threads");
                log(&paths, &format!("unnice {id} (focused hook): nice 0 on {n} threads"));
            }
            Err(e) => {
                eprintln!("{PLUGIN_ID}: {id}: {e}");
                log(&paths, &format!("unnice {id} FAILED: {e}"));
                ok = false;
            }
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
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
        (true, _) => format!("{}% {}", cpu_text(cpu), mem_text(rss)),
        (false, true) => format!("{}%", cpu_text(cpu)),
        (false, false) => mem_text(rss),
    }
}

/// Rounded DOWN, so the number never reaches a threshold before the colour does
/// (99.6 % stays "99%" in orange instead of a red-looking "100%").
fn cpu_text(cpu: f64) -> u64 {
    cpu.max(0.0).floor() as u64
}

fn mem_text(rss: u64) -> String {
    let mib = rss as f64 / 1_048_576.0;
    if mib >= 1024.0 {
        format!("{:.1}G", (mib / 1024.0 * 10.0).floor() / 10.0)
    } else {
        format!("{:.0}M", mib.floor())
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
    /// Highest nice value in the tree below the shell: "+5" means the throttle is on it.
    nice: i32,
    focused: bool,
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
            // Nice of the shell's direct children = the agent process itself (MCP servers
            // further down may carry their own values from tools like ananicy).
            let shell = pid.unwrap_or(0);
            let nice = snap.procs.iter().filter(|(_, p)| p.ppid == shell).map(|(_, p)| p.nice).max().unwrap_or(0);
            rows.push(Row {
                level: cfg.level(usage.cpu_pct, usage.rss_bytes),
                cpu: usage.cpu_pct,
                rss: usage.rss_bytes,
                procs: usage.procs,
                nice,
                focused: p.get("focused").and_then(Value::as_bool).unwrap_or(false),
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
        "{DIM}   {:>6} {:>7} {:>5} {:>4}  {:<7} {:<16} {:<40} {}{RESET}\n",
        "CPU", "RAM", "PROCS", "NICE", "AGENT", "SPACE", "PANE", "BUSIEST"
    ));
    for r in rows {
        let c = colour(r.level);
        let dot = if r.level == Level::Hidden { "·" } else { "●" };
        let nice = match r.nice {
            0 => String::new(),
            n if n > 0 => format!("+{n}"),
            n => n.to_string(),
        };
        let title = if r.focused { format!("▶ {}", r.title) } else { r.title.clone() };
        out.push_str(&format!(
            "{c}{dot}{RESET}  {c}{:>5}% {:>7}{RESET} {:>5} {:>4}  {:<7} {:<16} {:<40} {DIM}{}{RESET}\n",
            cpu_text(r.cpu),
            mem_text(r.rss),
            r.procs,
            nice,
            cut(if r.agent.is_empty() { "-" } else { &r.agent }, 7),
            cut(&r.space, 16),
            cut(&title, 40),
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
    if cfg.throttle_enabled() {
        out.push_str(&format!(
            "{DIM}Throttle: unfocused panes from {:.0} % CPU run at nice +{} (NICE column); ▶ marks the focused pane{RESET}\n",
            cfg.throttle_cpu, cfg.throttle_nice
        ));
    }
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
        assert_eq!(badge_text(3.0, 2867 << 20, false, false), "2.7G");
        assert_eq!(badge_text(99.6, 0, false, true), "99%");
        assert_eq!(badge_text(3.0, 2559 << 20, false, false), "2.4G");
    }

    #[test]
    fn throttle_only_busy_unfocused_panes_and_releases_on_focus() {
        use Throttle::*;
        // enabled, focused, throttled, cpu, from
        assert_eq!(throttle_step(true, false, false, 95.0, 90.0), Apply);
        assert_eq!(throttle_step(true, false, false, 89.9, 90.0), Keep);
        assert_eq!(throttle_step(true, true, false, 500.0, 90.0), Keep); // focused: never throttled
        assert_eq!(throttle_step(true, true, true, 500.0, 90.0), Release); // focus restores at once
        assert_eq!(throttle_step(true, false, true, 5.0, 90.0), Keep); // idle but unfocused: nice costs nothing
        assert_eq!(throttle_step(false, false, true, 500.0, 90.0), Release); // throttle switched off
        assert_eq!(throttle_step(false, false, false, 500.0, 90.0), Keep);
    }
}
