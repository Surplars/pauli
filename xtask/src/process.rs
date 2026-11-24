//! Single-child lifecycle for the resident console and deadline-bound smoke.
//! Ownership: caller owns stdin, reader owns stdout; only smoke shares the child with a watchdog.
//! Invariants: the watchdog can kill/reap the exact child even if serial I/O or logging blocks.
use anyhow::{Context, Result, ensure};
use std::{
    io::{self, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};
pub enum Output {
    Data(Vec<u8>),
    End,
    Idle,
}
pub struct Session {
    child: Arc<Mutex<Child>>,
    input: Option<ChildStdin>,
    output: Receiver<io::Result<Vec<u8>>>,
    watchdog: Option<Watchdog>,
}
struct Watchdog {
    deadline: Instant,
    cancel: mpsc::Sender<()>,
    worker: thread::JoinHandle<()>,
}

fn child_handle(child: &Mutex<Child>) -> MutexGuard<'_, Child> {
    child.lock().unwrap_or_else(|poison| poison.into_inner())
}
fn terminate(child: &Mutex<Child>) {
    let mut child = child_handle(child);
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
        let _ = child.wait();
    }
}
impl Session {
    pub fn start(command: &mut Command) -> Result<Self> {
        Self::launch(command, None)
    }
    pub fn start_until(command: &mut Command, deadline: Instant) -> Result<Self> {
        Self::launch(command, Some(deadline))
    }
    fn launch(command: &mut Command, deadline: Option<Instant>) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("launch qemu-system-riscv64")?;
        let (tx, output) = mpsc::sync_channel(32);
        let input = child.stdin.take();
        let stream = child.stdout.take();
        let mut session = Self {
            child: Arc::new(Mutex::new(child)),
            input,
            output,
            watchdog: None,
        };
        if let Some(deadline) = deadline {
            let (cancel, stop) = mpsc::channel();
            let child = Arc::clone(&session.child);
            let worker = thread::Builder::new()
                .name("hv-smoke-deadline".into())
                .spawn(move || {
                    if stop.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        == Err(RecvTimeoutError::Timeout)
                    {
                        terminate(&child);
                    }
                })?;
            session.watchdog = Some(Watchdog {
                deadline,
                cancel,
                worker,
            });
        }
        let mut stream = stream.context("QEMU stdout not piped")?;
        thread::Builder::new()
            .name("hv-serial-output".into())
            .spawn(move || {
                let mut bytes = [0; 4096];
                loop {
                    match stream.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(len) => {
                            if tx.send(Ok(bytes[..len].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            let _ = tx.send(Err(error));
                            break;
                        }
                    }
                }
            })?;
        Ok(session)
    }
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.input
            .as_mut()
            .context("QEMU stdin closed")?
            .write_all(bytes)?;
        Ok(())
    }
    pub fn close_input(&mut self) {
        self.input.take();
    }
    pub fn data(&self, delay: Duration) -> Result<Output> {
        Ok(match self.output.recv_timeout(delay) {
            Ok(result) => Output::Data(result?),
            Err(RecvTimeoutError::Timeout) => Output::Idle,
            Err(RecvTimeoutError::Disconnected) => Output::End,
        })
    }
    pub fn finish(&mut self, deadline: Option<Instant>) -> Result<()> {
        self.close_input();
        // Only a resident session may block in wait() holding the handle.
        if let Some(deadline) = self.watchdog.as_ref().map(|w| w.deadline).or(deadline) {
            loop {
                if let Some(status) = child_handle(&self.child).try_wait()? {
                    ensure!(status.success(), "QEMU exited with {status}");
                    return Ok(());
                }
                let left = deadline.saturating_duration_since(Instant::now());
                ensure!(!left.is_zero(), "QEMU shutdown deadline expired");
                thread::sleep(left.min(Duration::from_millis(10)));
            }
        }
        let status = child_handle(&self.child).wait()?;
        ensure!(status.success(), "QEMU exited with {status}");
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.close_input();
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.cancel.send(());
            let _ = watchdog.worker.join();
        }
        terminate(&self.child);
    }
}

#[cfg(test)]
mod tests;
