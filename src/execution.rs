//! Cooperative task boundaries and owned, bounded validation subprocesses.
use std::{
    io::{self, Read},
    process::{Child, Command, Output, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const CANCELLED: &str = "task cancelled";

#[derive(Clone)]
pub(crate) struct ExecutionControl {
    deadline: Instant,
    cancelled: Arc<dyn Fn() -> io::Result<bool> + Send + Sync>,
}

impl Default for ExecutionControl {
    fn default() -> Self {
        Self::new(Duration::from_secs(30 * 60), || Ok(false))
    }
}

impl ExecutionControl {
    pub(crate) fn new(
        timeout: Duration,
        cancelled: impl Fn() -> io::Result<bool> + Send + Sync + 'static,
    ) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            cancelled: Arc::new(cancelled),
        }
    }

    pub(crate) fn check(&self) -> io::Result<()> {
        if (self.cancelled)()? {
            return Err(io::Error::new(io::ErrorKind::Interrupted, CANCELLED));
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "task runtime budget exceeded",
            ));
        }
        Ok(())
    }

    pub(crate) fn output(&self, command: &mut Command) -> io::Result<Output> {
        self.check()?;
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        let mut child = OwnedChild::spawn(command)?;
        let stdout = child.child.stdout.take().expect("piped stdout");
        let stderr = child.child.stderr.take().expect("piped stderr");
        let out_reader = thread::spawn(move || capture(stdout));
        let err_reader = thread::spawn(move || capture(stderr));
        let status = loop {
            if let Err(error) = self.check() {
                child.terminate();
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(error);
            }
            match child.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(error) => {
                    child.terminate();
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err(error);
                }
            }
        };
        // Descendants must not outlive the command or keep capture pipes open.
        child.terminate();
        let stdout = out_reader
            .join()
            .map_err(|_| io::Error::other("stdout reader stopped"))??;
        let stderr = err_reader
            .join()
            .map_err(|_| io::Error::other("stderr reader stopped"))??;
        self.check()?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

fn capture(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let retained = read.min(MAX_CAPTURE_BYTES.saturating_sub(result.len()));
        result.extend_from_slice(&buffer[..retained]);
    }
    Ok(result)
}

pub(crate) struct OwnedChild {
    pub(crate) child: Child,
    terminated: bool,
}
impl OwnedChild {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        Ok(Self {
            child: command.spawn()?,
            terminated: false,
        })
    }
    fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        #[cfg(unix)]
        if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn timeout_terminates_command_and_descendant() {
        let control = ExecutionControl::new(Duration::from_millis(100), || Ok(false));
        let start = Instant::now();
        let error = control
            .output(Command::new("sh").args(["-c", "sleep 60 & wait"]))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn running_cancellation_kills_process_and_prevents_next_command() {
        let directory = tempfile::tempdir().unwrap();
        let barrier = directory.path().join("started");
        let flag = Arc::new(AtomicBool::new(false));
        let observer = flag.clone();
        let control = ExecutionControl::new(Duration::from_secs(3), move || {
            Ok(observer.load(Ordering::Acquire))
        });
        let watcher = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !barrier.exists() {
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
            flag.store(true, Ordering::Release);
        });
        let error = control
            .output(
                Command::new("sh")
                    .args(["-c", "touch \"$1\"; sleep 60 & wait", "sh"])
                    .arg(directory.path().join("started")),
            )
            .unwrap_err();
        watcher.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(
            control
                .output(
                    Command::new("sh")
                        .args(["-c", "touch \"$1\"", "sh"])
                        .arg(directory.path().join("next"))
                )
                .is_err()
        );
        assert!(!directory.path().join("next").exists());
    }
}
