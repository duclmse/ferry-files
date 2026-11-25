//! `fsd stop`: a clean stop of the daemon that uses a config's data folder,
//! as Ctrl-C gives one running in a console. It saves the memory store and
//! commits the full-text index on the way out, which killing it does not.
//!
//! Unix sends SIGTERM to the PID in `data_dir/fs.pid`. Windows has no such
//! signal for a process without a console window of its own, so the daemon
//! waits on a named event beside its instance mutex and `stop` sets it. The
//! event gets the creator's default security: a service's can be set only by
//! an administrator, a user's daemon's only by that user.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It stopped within the wait.
    Stopped,
    /// It was asked to stop and is still shutting down.
    Stopping,
    NotRunning,
}

/// Asks the daemon using `data_dir` to stop, and waits up to `wait` for it.
pub fn stop(data_dir: &Path, wait: Duration) -> Result<Outcome, AppError> {
    if !imp::request(data_dir)? {
        return Ok(Outcome::NotRunning);
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if !imp::running(data_dir) {
            return Ok(Outcome::Stopped);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(Outcome::Stopping)
}

#[cfg(windows)]
pub(crate) use imp::listen;

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use tokio_util::sync::CancellationToken;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        CreateEventW, OpenEventW, OpenMutexW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
        SYNCHRONIZATION_SYNCHRONIZE,
    };

    use crate::error::AppError;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn event_name(data_dir: &Path) -> Vec<u16> {
        let mutex = crate::daemon::instance_mutex_name(data_dir);
        wide(&mutex.replacen("fs_daemon_", "fs_daemon_stop_", 1))
    }

    struct Owned(HANDLE);

    // SAFETY: an event handle may be waited on and closed from any thread;
    // `listen` hands its one to the thread that waits on it.
    unsafe impl Send for Owned {}

    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Daemon side: cancels `shutdown` when `stop` sets the event. The
    /// thread ends with the daemon, so tests that run several leave none.
    pub fn listen(data_dir: &Path, shutdown: CancellationToken) -> Result<(), AppError> {
        let name = event_name(data_dir);
        let event = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }
            .map_err(|e| AppError::Config(format!("CreateEventW: {e}")))?;
        let event = Owned(event);
        std::thread::Builder::new()
            .name("fsd-stop".into())
            .spawn(move || {
                // The whole guard, not just its handle, so it closes here.
                let event = event;
                while !shutdown.is_cancelled() {
                    if unsafe { WaitForSingleObject(event.0, 500) } == WAIT_OBJECT_0 {
                        tracing::info!("stop requested, shutting down");
                        shutdown.cancel();
                    }
                }
            })
            .map_err(AppError::Io)?;
        Ok(())
    }

    /// Sets the event; false when no daemon made it.
    pub fn request(data_dir: &Path) -> Result<bool, AppError> {
        let name = event_name(data_dir);
        let event = match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) } {
            Ok(h) => Owned(h),
            Err(e) if e.code() == windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult() => {
                return Err(AppError::Config(
                    "not allowed to stop this fsd: a service is stopped from an elevated \
                     prompt (Stop-Service FsDaemon)"
                        .into(),
                ))
            }
            Err(_) => return Ok(false),
        };
        unsafe { SetEvent(event.0) }.map_err(|e| AppError::Config(format!("SetEvent: {e}")))?;
        Ok(true)
    }

    /// Whether the instance mutex is still held.
    pub fn running(data_dir: &Path) -> bool {
        let name = wide(&crate::daemon::instance_mutex_name(data_dir));
        match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) } {
            Ok(h) => {
                drop(Owned(h));
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::path::Path;

    use nix::errno::Errno;
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    use crate::error::AppError;

    fn pid(data_dir: &Path) -> Option<Pid> {
        let text = std::fs::read_to_string(data_dir.join("fs.pid")).ok()?;
        text.trim().parse().ok().map(Pid::from_raw)
    }

    /// Sends SIGTERM; false when the PID file names no live process.
    pub fn request(data_dir: &Path) -> Result<bool, AppError> {
        let Some(pid) = pid(data_dir) else {
            return Ok(false);
        };
        match kill(pid, Signal::SIGTERM) {
            Ok(()) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(Errno::EPERM) => Err(AppError::Config(format!(
                "not allowed to stop fsd (PID {pid}); a service is stopped with systemctl \
                 or launchctl"
            ))),
            Err(e) => Err(AppError::Config(format!("kill {pid}: {e}"))),
        }
    }

    pub fn running(data_dir: &Path) -> bool {
        pid(data_dir).is_some_and(|p| kill(p, None).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_stop_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            stop(dir.path(), Duration::from_millis(10)).unwrap(),
            Outcome::NotRunning
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_listening_daemon_is_told_to_stop() {
        let dir = tempfile::tempdir().unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        listen(dir.path(), shutdown.clone()).unwrap();
        assert!(imp::request(dir.path()).unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !shutdown.is_cancelled() {
            assert!(Instant::now() < deadline, "not cancelled");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
