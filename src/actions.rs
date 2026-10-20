//! Process actions (end, set priority, open file location).
//!
//! Each request runs on its own short-lived thread so the UI never waits on
//! a Win32 call, and reports back a human-readable `Outcome` for a toast.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME, HANDLE,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, SetPriorityClass, TerminateProcess, WaitForSingleObject,
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, PROCESS_ACCESS_RIGHTS,
    PROCESS_CREATION_FLAGS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

use crate::model::ProcessInfo;

/// Processes whose termination would crash or log off Windows. Ending them
/// requires admin anyway; we refuse outright instead of trying.
const CRITICAL: [&str; 10] = [
    "System",
    "System Idle Process",
    "Registry",
    "Secure System",
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
];

/// How long to wait for ended processes to actually exit before reporting.
const EXIT_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct ProcTarget {
    pub pid: u32,
    /// Unix seconds; checked against the live process to avoid acting on a
    /// reused PID. 0 = unknown, check skipped.
    pub start_time: u64,
    pub name: String,
}

impl ProcTarget {
    pub fn of(p: &ProcessInfo) -> Self {
        Self {
            pid: p.pid,
            start_time: p.start_time,
            name: p.name.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
}

impl Priority {
    pub const ALL: [Priority; 5] = [
        Self::High,
        Self::AboveNormal,
        Self::Normal,
        Self::BelowNormal,
        Self::Idle,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Low",
            Self::BelowNormal => "Below normal",
            Self::Normal => "Normal",
            Self::AboveNormal => "Above normal",
            Self::High => "High",
        }
    }

    fn class(self) -> PROCESS_CREATION_FLAGS {
        match self {
            Self::Idle => IDLE_PRIORITY_CLASS,
            Self::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            Self::Normal => NORMAL_PRIORITY_CLASS,
            Self::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
            Self::High => HIGH_PRIORITY_CLASS,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Request {
    End(Vec<ProcTarget>),
    SetPriority(Vec<ProcTarget>, Priority),
    OpenLocation(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub message: String,
    pub is_error: bool,
}

/// Run `request` in the background; `done` receives the outcome (`None` if
/// there is nothing worth telling the user).
pub fn spawn(request: Request, done: impl FnOnce(Option<Outcome>) + Send + 'static) {
    thread::Builder::new()
        .name("process-action".into())
        .spawn(move || done(run(request)))
        .expect("failed to spawn action thread");
}

fn run(request: Request) -> Option<Outcome> {
    match request {
        Request::End(targets) => Some(end_processes(&targets)),
        Request::SetPriority(targets, priority) => Some(set_priority(&targets, priority)),
        Request::OpenLocation(path) => open_location(&path),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    AccessDenied,
    Gone,
    Critical,
    Other(u32),
}

impl Failure {
    fn last_error() -> Self {
        match unsafe { GetLastError() } {
            ERROR_ACCESS_DENIED => Self::AccessDenied,
            // OpenProcess reports a PID that no longer exists this way.
            ERROR_INVALID_PARAMETER => Self::Gone,
            code => Self::Other(code),
        }
    }

    fn describe(self) -> String {
        match self {
            Self::AccessDenied => "access denied (try running as administrator)".into(),
            Self::Gone => "the process has already exited".into(),
            Self::Critical => "it is a critical system process".into(),
            Self::Other(code) => format!("Windows error {code}"),
        }
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn is_critical(t: &ProcTarget) -> bool {
    t.pid == 0 || t.pid == 4 || CRITICAL.iter().any(|c| c.eq_ignore_ascii_case(&t.name))
}

/// Open `t` with `access`, verifying it is still the same process.
fn open(t: &ProcTarget, access: PROCESS_ACCESS_RIGHTS) -> Result<OwnedHandle, Failure> {
    let handle = unsafe { OpenProcess(access | PROCESS_QUERY_LIMITED_INFORMATION, 0, t.pid) };
    if handle.is_null() {
        return Err(Failure::last_error());
    }
    let handle = OwnedHandle(handle);
    if t.start_time != 0 {
        if let Some(created) = creation_unix_secs(&handle) {
            if created.abs_diff(t.start_time) > 1 {
                return Err(Failure::Gone);
            }
        }
    }
    Ok(handle)
}

fn creation_unix_secs(handle: &OwnedHandle) -> Option<u64> {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600; // 1601-01-01 -> 1970-01-01
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    let ok = unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Some((ticks / 10_000_000).saturating_sub(EPOCH_DIFF_SECS))
}

fn end_processes(targets: &[ProcTarget]) -> Outcome {
    let mut results = Vec::with_capacity(targets.len());
    let mut ended = Vec::new();
    for t in targets {
        if is_critical(t) {
            results.push(Err(Failure::Critical));
            continue;
        }
        let result = open(t, PROCESS_TERMINATE | PROCESS_SYNCHRONIZE).and_then(|h| {
            if unsafe { TerminateProcess(h.0, 1) } != 0 {
                ended.push(h);
                Ok(())
            } else {
                Err(Failure::last_error())
            }
        });
        results.push(result);
    }

    // Termination is asynchronous; wait (bounded) so the refresh that
    // follows no longer shows the processes.
    let deadline = Instant::now() + EXIT_WAIT;
    for h in &ended {
        let left = deadline.saturating_duration_since(Instant::now());
        unsafe { WaitForSingleObject(h.0, left.as_millis() as u32) };
    }

    summarize(targets, &results, "Ended", "end")
}

fn set_priority(targets: &[ProcTarget], priority: Priority) -> Outcome {
    let results: Vec<_> = targets
        .iter()
        .map(|t| {
            let h = open(t, PROCESS_SET_INFORMATION)?;
            if unsafe { SetPriorityClass(h.0, priority.class()) } != 0 {
                Ok(())
            } else {
                Err(Failure::last_error())
            }
        })
        .collect();
    let done = format!("Set {} priority for", priority.label());
    summarize(targets, &results, &done, "change the priority of")
}

fn open_location(path: &Path) -> Option<Outcome> {
    // explorer needs the quotes inside the /select argument, so build the
    // command line ourselves instead of letting std quote it.
    match Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
    {
        Ok(_) => None,
        Err(e) => Some(Outcome {
            message: format!("Could not open file location: {e}"),
            is_error: true,
        }),
    }
}

fn summarize(
    targets: &[ProcTarget],
    results: &[Result<(), Failure>],
    done: &str,
    attempt: &str,
) -> Outcome {
    if let ([t], [result]) = (targets, results) {
        let what = format!("{} (PID {})", t.name, t.pid);
        return match result {
            Ok(()) => Outcome {
                message: format!("{done} {what}"),
                is_error: false,
            },
            Err(f) => Outcome {
                message: format!("Could not {attempt} {what}: {}", f.describe()),
                is_error: *f != Failure::Gone,
            },
        };
    }

    let ok = results.iter().filter(|r| r.is_ok()).count();
    let count = |want: fn(&Failure) -> bool| {
        results
            .iter()
            .filter(|r| matches!(r, Err(f) if want(f)))
            .count()
    };
    let denied = count(|f| *f == Failure::AccessDenied);
    let gone = count(|f| *f == Failure::Gone);
    let critical = count(|f| *f == Failure::Critical);
    let other = count(|f| matches!(f, Failure::Other(_)));

    let mut message = format!("{done} {ok} of {} processes", targets.len());
    let mut notes = Vec::new();
    if denied > 0 {
        notes.push(format!("{denied} access denied (try running as administrator)"));
    }
    if critical > 0 {
        notes.push(format!("{critical} critical system processes skipped"));
    }
    if gone > 0 {
        notes.push(format!("{gone} already exited"));
    }
    if other > 0 {
        notes.push(format!("{other} failed"));
    }
    if !notes.is_empty() {
        message.push_str(" — ");
        message.push_str(&notes.join(", "));
    }
    Outcome {
        message,
        is_error: denied + critical + other > 0,
    }
}
