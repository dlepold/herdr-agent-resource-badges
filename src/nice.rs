// Nice-value control for the throttle: a busy pane without focus gets its process
// tree set to a higher nice value (less CPU share under contention); when the pane
// is focused again the tree goes back to nice 0.
// Created 2026-10-23 (David Lepold + Claude Fable 5.1). Last update: 2026-10-23.
//
// Raising nice is allowed for any process of the same user. LOWERING it needs
// CAP_SYS_NICE, because RLIMIT_NICE is 0 almost everywhere. Three ways, tried in order:
//   direct  — this binary carries the capability:  sudo setcap cap_sys_nice+ep <binary>
//   sudo    — passwordless sudo for this binary:    sudo -n <binary> setnice 0 <pids…>
//   none    — the throttle stays off and the sampler logs why, once.
// `probe_lower` finds out which way works on a throw-away `sleep` child, never on the
// sampler itself, so a failed probe leaves nothing in a changed state.

use std::env;
use std::io;
use std::process::{Command, ExitCode, Stdio};

const PRIO_PROCESS: i32 = 0;

extern "C" {
    fn setpriority(which: i32, who: u32, prio: i32) -> i32;
}

/// Sets the nice value of one process. Raising always works for own processes;
/// lowering returns EPERM without CAP_SYS_NICE.
pub fn set(pid: u32, nice: i32) -> io::Result<()> {
    // SAFETY: plain syscall wrapper with scalar arguments; no memory is shared.
    if unsafe { setpriority(PRIO_PROCESS, pid, nice) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Linux nice values belong to individual threads. Inspect every task even when
/// the leader already has the requested priority (workers may predate the change).
pub fn matching_tasks(pid: u32, matches: impl Fn(i32) -> bool) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    let tids: Vec<u32> = std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    #[cfg(not(target_os = "linux"))]
    let tids = vec![pid];
    tids.into_iter()
        .filter(|&tid| crate::sample::read_proc(tid).is_some_and(|p| matches(p.nice)))
        .collect()
}

/// How this process can lower a nice value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lower {
    Direct,
    Sudo,
    Unavailable(String),
}

impl Lower {
    pub fn label(&self) -> &str {
        match self {
            Lower::Direct => "direct (CAP_SYS_NICE)",
            Lower::Sudo => "sudo -n (passwordless)",
            Lower::Unavailable(_) => "unavailable",
        }
    }
}

/// Finds out, on a throw-away child, whether a raised nice value can be lowered again.
pub fn probe_lower() -> Lower {
    let mut child = match Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Lower::Unavailable(format!("cannot spawn probe child: {e}")),
    };
    let pid = child.id();
    let result = (|| {
        set(pid, 1).map_err(|e| format!("raising nice failed: {e}"))?;
        if set(pid, 0).is_ok() {
            return Ok(Lower::Direct);
        }
        match sudo_setnice(&[pid], 0) {
            Ok(()) if crate::sample::read_proc(pid).map(|p| p.nice) == Some(0) => Ok(Lower::Sudo),
            Ok(()) => Err("sudo helper ran but the nice value did not change".to_string()),
            Err(e) => Err(format!(
                "cannot lower nice: {e}. Grant it with  sudo setcap cap_sys_nice+ep <binary>  \
                 or passwordless sudo for  <binary> setnice"
            )),
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    result.unwrap_or_else(Lower::Unavailable)
}

/// Lowers `pids` to `nice` the way `how` says. Returns how many were changed.
pub fn lower(pids: &[u32], nice: i32, how: &Lower) -> Result<usize, String> {
    if pids.is_empty() {
        return Ok(0);
    }
    match how {
        Lower::Direct => Ok(pids.iter().filter(|&&pid| set(pid, nice).is_ok()).count()),
        Lower::Sudo => sudo_setnice(pids, nice).map(|_| pids.len()),
        Lower::Unavailable(why) => Err(why.clone()),
    }
}

/// Tries the direct way first and falls back to sudo for the PIDs that refused.
/// Used by the `unnice` hook, which runs too briefly for a probe.
pub fn lower_best_effort(pids: &[u32], nice: i32) -> Result<usize, String> {
    let refused: Vec<u32> = pids.iter().copied().filter(|&pid| set(pid, nice).is_err()).collect();
    if refused.is_empty() {
        return Ok(pids.len());
    }
    sudo_setnice(&refused, nice).map(|_| pids.len())
}

/// `sudo -n <own binary> setnice <nice> <pids…>` — root runs our own `setnice`
/// subcommand, so no external tool's option semantics are involved.
fn sudo_setnice(pids: &[u32], nice: i32) -> Result<(), String> {
    let exe = env::current_exe().map_err(|e| format!("own binary: {e}"))?;
    let out = Command::new("sudo")
        .arg("-n")
        .arg(&exe)
        .arg("setnice")
        .arg(nice.to_string())
        .args(pids.iter().map(u32::to_string))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("sudo: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("sudo -n {} setnice: {}", exe.display(), err.trim()))
    }
}

/// The root helper behind `sudo -n <binary> setnice <nice> <pid>…`. It only ever
/// touches the PIDs it is given and reports every failure on stderr.
pub fn setnice_cli(args: &[String]) -> ExitCode {
    let Some(nice) = args.first().and_then(|a| a.parse::<i32>().ok()) else {
        eprintln!("usage: setnice <nice -20..19> <pid>…");
        return ExitCode::from(2);
    };
    let mut failed = 0;
    for a in &args[1..] {
        match a.parse::<u32>() {
            Ok(pid) => {
                if let Err(e) = set(pid, nice) {
                    eprintln!("pid {pid}: {e}");
                    failed += 1;
                }
            }
            Err(_) => {
                eprintln!("not a pid: {a}");
                failed += 1;
            }
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_workers_when_leader_is_already_throttled() {
        use std::io::{BufRead, BufReader};
        let mut child = Command::new("python3")
            .args(["-u", "-c", "import threading,time; [threading.Thread(target=time.sleep,args=(15,),daemon=True).start() for _ in range(3)]; print('ready'); time.sleep(15)"])
            .stdout(Stdio::piped()).spawn().expect("spawn threaded child");
        let pid = child.id();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut ready = String::new();
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready).unwrap();
            assert_eq!(ready.trim(), "ready");
            set(pid, 5).unwrap();
            let workers = matching_tasks(pid, |n| n < 5);
            assert_eq!(workers.len(), 3);
            assert!(!workers.contains(&pid));
            for tid in workers { set(tid, 5).unwrap(); }
            assert!(matching_tasks(pid, |n| n < 5).is_empty());
            assert_eq!(matching_tasks(pid, |n| n == 5).len(), 4);
        }));
        let _ = child.kill();
        let _ = child.wait();
        if let Err(e) = result { std::panic::resume_unwind(e); }
    }

    #[test]
    fn raising_own_child_works_without_privileges() {
        let mut child = Command::new("sleep").arg("5").spawn().expect("spawn");
        let pid = child.id();
        assert!(set(pid, 3).is_ok());
        assert_eq!(crate::sample::read_proc(pid).map(|p| p.nice), Some(3));
        let _ = child.kill();
        let _ = child.wait();
    }
}
