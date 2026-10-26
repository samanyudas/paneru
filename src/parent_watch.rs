//! Ends the daemon when the app that started it is gone.
//!
//! Miri starts the engine as a child process and sets `PANERU_PARENT_PID`.
//! If Miri dies without stopping the engine (a crash, `kill`, or Force Quit),
//! the engine would otherwise keep managing windows that no app controls.

use std::io;
use std::thread;

use tracing::{error, info};

use crate::events::{Event, EventSender};

const PARENT_PID_VARIABLE: &str = "PANERU_PARENT_PID";

/// Sends `Event::Exit` when the process named by `PANERU_PARENT_PID` exits,
/// so the normal exit cleanup runs. Does nothing when the variable is unset.
pub fn exit_with_parent(sender: EventSender) {
    let Some(raw) = std::env::var_os(PARENT_PID_VARIABLE) else {
        return;
    };
    let Some(pid) = raw.to_str().and_then(|raw| raw.parse::<u32>().ok()) else {
        error!("{PARENT_PID_VARIABLE} is {raw:?}, which is not a process ID");
        return;
    };
    thread::spawn(move || match wait_for_exit(pid) {
        Ok(()) => {
            info!("parent process {pid} exited, stopping");
            let _ = sender.send(Event::Exit);
        }
        Err(err) => error!("cannot watch parent process {pid}: {err}"),
    });
}

/// Blocks until process `pid` exits. Returns at once when it is already gone.
fn wait_for_exit(pid: u32) -> io::Result<()> {
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = wait_on_queue(queue, pid);
    unsafe { libc::close(queue) };
    result
}

fn wait_on_queue(queue: libc::c_int, pid: u32) -> io::Result<()> {
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let registered = unsafe {
        libc::kevent(
            queue,
            &raw const change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if registered < 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ESRCH) => Ok(()),
            _ => Err(err),
        };
    }
    let mut event = unsafe { std::mem::zeroed::<libc::kevent>() };
    loop {
        let count = unsafe {
            libc::kevent(
                queue,
                std::ptr::null(),
                0,
                &raw mut event,
                1,
                std::ptr::null(),
            )
        };
        if count > 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if count < 0 && err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::wait_for_exit;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn returns_when_the_watched_process_exits() {
        let mut child = Command::new("/bin/sleep").arg("0.3").spawn().unwrap();
        let pid = child.id();
        let reaper = std::thread::spawn(move || child.wait());
        let start = Instant::now();
        wait_for_exit(pid).unwrap();
        let waited = start.elapsed();
        reaper.join().unwrap().unwrap();
        assert!(
            waited >= Duration::from_millis(200),
            "returned after {waited:?}, before the exit"
        );
        assert!(waited < Duration::from_secs(5), "returned after {waited:?}");
    }

    #[test]
    fn returns_at_once_for_a_process_that_is_gone() {
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        wait_for_exit(pid).unwrap();
    }
}
