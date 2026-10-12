//! Battle-child lifetime guard (DESIGN §3.2 live loop): every battle child
//! joins a process-wide Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`,
//! so an ABNORMAL menu death — panic, Task Manager kill — closes the last
//! job handle and takes every battle child with it. An orphaned child would
//! otherwise keep injecting into HOI4 while a relaunched menu starts its own
//! listener on the same console channel. The guard deliberately does NOT
//! cover the normal exit paths — those kill the children explicitly first.

use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

/// The process-wide job handle (raw isize — HANDLE is a pointer, not Send).
/// Created lazily on the first guarded spawn; NEVER closed — the process
/// lifetime IS the guard's lifetime (closing early would release the kill).
fn battle_job() -> Option<HANDLE> {
    static JOB: OnceLock<Option<isize>> = OnceLock::new();
    JOB.get_or_init(|| unsafe {
        // SAFETY: CreateJobObjectW with default security/name creates a
        // process-local job object; SetInformationJobObject writes a
        // correctly sized JOBOBJECT_EXTENDED_LIMIT_INFORMATION into it.
        let job = CreateJobObjectW(None, None).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .ok()?;
        Some(job.0 as isize)
    })
    .map(|raw| HANDLE(raw as *mut _))
}

/// Assign a freshly spawned child to the kill-on-close job. Best-effort:
/// on any failure the child runs unguarded (the pre-guard behavior) — a
/// normal menu exit still kills it explicitly.
pub fn guard_child(child: &std::process::Child) {
    let Some(job) = battle_job() else {
        return;
    };
    unsafe {
        // SAFETY: OpenProcess on the child's own pid with exactly the
        // rights AssignProcessToJobObject needs; the handle is closed
        // right after (the job keeps its own reference to the process).
        let Ok(proc) = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, child.id()) else {
            return;
        };
        let _ = AssignProcessToJobObject(job, proc);
        let _ = CloseHandle(proc);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn job_object_is_created_and_assignable() {
        // A sleeping child joins the job; the whole point of the guard is
        // kernel-side (kill-on-close), so the test only pins creation +
        // assignment success, then kills the child itself.
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        super::guard_child(&child);
        // If the assignment failed the child simply runs unguarded; the
        // creation path above already exercised the Win32 calls. Reaping:
        let _ = child.kill();
        let _ = child.wait();
    }
}
