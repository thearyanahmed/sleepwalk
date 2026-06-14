//! Spawning and reaping the Firecracker process.
//!
//! Each microVM is a separate `firecracker` process listening on its own API
//! socket; the [`Firecracker`](crate::firecracker::Firecracker) client drives it
//! over that socket. [`FcProcess`] owns the child: it launches the binary,
//! redirects the guest serial console to a log file, waits for the API socket to
//! appear before returning (so the first client call cannot race the socket),
//! and kills + reaps the process on [`kill`](FcProcess::kill) or drop.
//!
//! Jailer-based confinement (running Firecracker inside a chroot/cgroup/netns) is
//! a later hardening step; this is the plain-spawn path.

use std::fs::File;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// A running Firecracker child process bound to one API socket.
#[derive(Debug)]
pub struct FcProcess {
    child: Child,
}

impl FcProcess {
    /// Launch `bin` with `--api-sock api_sock`, sending the guest serial console
    /// (Firecracker's stdout/stderr) to `serial_log`, and wait up to `timeout`
    /// for the API socket to appear.
    ///
    /// # Errors
    /// If the binary cannot be spawned, exits before the socket appears, or the
    /// socket does not appear within `timeout`.
    pub fn spawn(
        bin: &Path,
        api_sock: &Path,
        serial_log: &Path,
        timeout: Duration,
    ) -> io::Result<Self> {
        // A stale socket from a prior run would make the wait return instantly
        // against a dead endpoint.
        let _ = std::fs::remove_file(api_sock);

        let log = File::create(serial_log)?;
        let log_err = log.try_clone()?;
        let child = Command::new(bin)
            .arg("--api-sock")
            .arg(api_sock)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err)
            .spawn()?;

        let mut proc = Self { child };
        proc.wait_for_socket(api_sock, timeout)?;
        Ok(proc)
    }

    /// The child's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Kill the process and reap it. Idempotent enough for `Drop` to call.
    ///
    /// # Errors
    /// If reaping the process fails.
    pub fn kill(&mut self) -> io::Result<()> {
        let _ = self.child.kill();
        self.child.wait()?;
        Ok(())
    }

    fn wait_for_socket(&mut self, sock: &Path, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if sock.exists() {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                return Err(io::Error::other(format!(
                    "firecracker exited before its API socket appeared: {status}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "firecracker API socket did not appear in time",
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for FcProcess {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}
