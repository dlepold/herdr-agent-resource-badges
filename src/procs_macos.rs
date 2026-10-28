// macOS process reader: libproc (proc_listallpids + proc_pidinfo), the interface
// `top` and Activity Monitor use. No crates; the two structs below mirror
// <sys/proc_info.h> and are pinned by compile-time size checks.
// Created 2026-10-22 (David Lepold + Claude Opus 5.5). Last update: 2026-10-22.
//
// Notes:
// - CPU times in proc_taskinfo are Mach absolute time units, not nanoseconds. On
//   Apple Silicon one unit is 125/3 ns; mach_timebase_info gives the ratio.
// - Processes of other users (root daemons) are not readable without privileges;
//   they are simply skipped, which is fine for an agent's own process tree.

use super::Proc;
use std::collections::HashMap;
use std::ffi::{c_int, c_void};
use std::mem::{size_of, MaybeUninit};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const PROC_PIDTASKALLINFO: c_int = 2;
const MAXCOMLEN: usize = 16;
const NODEV: u32 = u32::MAX;

#[allow(dead_code)] // mirrors the C struct; most fields are unused
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [u8; MAXCOMLEN],
    pbi_name: [u8; 2 * MAXCOMLEN],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

#[allow(dead_code)] // mirrors the C struct; most fields are unused
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcTaskInfo {
    pti_virtual_size: u64,
    pti_resident_size: u64,
    pti_total_user: u64,
    pti_total_system: u64,
    pti_threads_user: u64,
    pti_threads_system: u64,
    pti_policy: i32,
    pti_faults: i32,
    pti_pageins: i32,
    pti_cow_faults: i32,
    pti_messages_sent: i32,
    pti_messages_received: i32,
    pti_syscalls_mach: i32,
    pti_syscalls_unix: i32,
    pti_csw: i32,
    pti_threadnum: i32,
    pti_numrunning: i32,
    pti_priority: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcTaskAllInfo {
    pbsd: ProcBsdInfo,
    ptinfo: ProcTaskInfo,
}

// Sizes from <sys/proc_info.h>; proc_pidinfo only succeeds with the exact size.
const _: () = assert!(size_of::<ProcBsdInfo>() == 136);
const _: () = assert!(size_of::<ProcTaskInfo>() == 96);
const _: () = assert!(size_of::<ProcTaskAllInfo>() == 232);

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn proc_listallpids(buffer: *mut c_void, buffersize: c_int) -> c_int;
    fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buffer: *mut c_void, buffersize: c_int) -> c_int;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
}

fn timebase() -> (u64, u64) {
    static T: OnceLock<(u64, u64)> = OnceLock::new();
    *T.get_or_init(|| {
        let mut tb = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: mach_timebase_info writes two u32 into the struct we pass.
        let rc = unsafe { mach_timebase_info(&mut tb) };
        if rc == 0 && tb.numer > 0 && tb.denom > 0 {
            (tb.numer as u64, tb.denom as u64)
        } else {
            (1, 1)
        }
    })
}

fn task_all_info(pid: u32) -> Option<ProcTaskAllInfo> {
    let size = size_of::<ProcTaskAllInfo>() as c_int;
    let mut info = MaybeUninit::<ProcTaskAllInfo>::zeroed();
    // SAFETY: the buffer is exactly `size` bytes and plain-old-data; the kernel
    // fills it completely when it returns `size`, which is checked before use.
    let got = unsafe { proc_pidinfo(pid as c_int, PROC_PIDTASKALLINFO, 0, info.as_mut_ptr().cast(), size) };
    if got != size {
        return None;
    }
    Some(unsafe { info.assume_init() })
}

fn c_name(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn to_proc(info: &ProcTaskAllInfo) -> Proc {
    let (numer, denom) = timebase();
    let mach = info.ptinfo.pti_total_user.saturating_add(info.ptinfo.pti_total_system);
    let name = c_name(&info.pbsd.pbi_name);
    Proc {
        ppid: info.pbsd.pbi_ppid,
        cpu_ns: (mach as u128 * numer as u128 / denom as u128) as u64,
        start: info.pbsd.pbi_start_tvsec.saturating_mul(1_000_000) + info.pbsd.pbi_start_tvusec,
        rss_bytes: info.ptinfo.pti_resident_size,
        nice: info.pbsd.pbi_nice,
        name: if name.is_empty() { c_name(&info.pbsd.pbi_comm) } else { name },
    }
}

pub fn scan() -> HashMap<u32, Proc> {
    let mut procs = HashMap::new();
    // SAFETY: a null buffer asks only for the current number of PIDs.
    let estimate = unsafe { proc_listallpids(std::ptr::null_mut(), 0) };
    if estimate <= 0 {
        return procs;
    }
    // Headroom for processes started between the two calls.
    let mut pids = vec![0 as c_int; estimate as usize + 128];
    let bytes = (pids.len() * size_of::<c_int>()) as c_int;
    // SAFETY: `pids` holds `bytes` bytes; the call returns how many PIDs it wrote.
    let count = unsafe { proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if count <= 0 {
        return procs;
    }
    procs.reserve(count as usize);
    for &pid in &pids[..(count as usize).min(pids.len())] {
        if pid <= 0 {
            continue;
        }
        if let Some(info) = task_all_info(pid as u32) {
            procs.insert(pid as u32, to_proc(&info));
        }
    }
    procs
}

pub fn read_proc(pid: u32) -> Option<Proc> {
    task_all_info(pid).map(|info| to_proc(&info))
}

/// Start times are microseconds since the Unix epoch, so "now" is the wall clock.
pub fn start_marker_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

/// True when the process has a controlling terminal.
pub fn has_terminal(pid: u32) -> bool {
    task_all_info(pid).is_some_and(|i| i.pbsd.e_tdev != NODEV && i.pbsd.e_tdev != 0)
}
