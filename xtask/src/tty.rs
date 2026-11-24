//! Resident host console, with bounded replay for detached guests.
//! Ownership/Lock Ordering: main thread owns the child, serial decoder and terminal mode.
//! Invariants: output is shown only for the host or a matching attached epoch.
use crate::process::{Output, Session};
use anyhow::{Result, ensure};
use pauli_devices::console::{Decoder, Event, Source};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, IsTerminal, Read, Write},
    process::Command,
    sync::mpsc,
    thread,
    time::Duration,
};
const REPLAY: usize = 16 * 1024;
// A terminal replay budget, not a limit on the number of manageable VMs.
const REPLAY_EPOCHS: usize = (1024 * 1024) / REPLAY;
pub fn run(command: &mut Command) -> Result<()> {
    let _mode = if io::stdin().is_terminal() {
        Some(mode::Mode::raw()?)
    } else {
        None
    };
    let mut session = Session::start(command)?;
    let (tx, rx) = mpsc::sync_channel(8);
    thread::Builder::new()
        .name("hv-terminal-input".into())
        .spawn(move || {
            let mut input = io::stdin().lock();
            let mut bytes = [0; 256];
            while let Ok(count) = input.read(&mut bytes) {
                if count == 0 || tx.send(bytes[..count].to_vec()).is_err() {
                    break;
                }
            }
        })?;
    let mut decoder = Decoder::new();
    let mut view = View::default();
    let mut out = io::stdout().lock();
    loop {
        for bytes in rx.try_iter().take(8) {
            session.send(&bytes)?;
        }
        match session.data(Duration::from_millis(8))? {
            Output::Data(bytes) => view.render(&mut out, &mut decoder, bytes)?,
            Output::Idle => {}
            Output::End => break,
        }
    }
    ensure!(decoder.complete(), "truncated serial frame");
    view.text.finish(&mut out)?;
    session.finish(None)
}

#[derive(Default)]
struct View {
    attached: Option<u32>,
    logs: BTreeMap<u32, VecDeque<u8>>,
    text: Text,
    source: Option<Source>,
}
impl View {
    fn render(
        &mut self,
        out: &mut impl Write,
        decoder: &mut Decoder,
        input: Vec<u8>,
    ) -> Result<()> {
        let mut visible = Vec::new();
        for byte in input {
            match decoder
                .push(byte)
                .map_err(|_| anyhow::anyhow!("bad serial framing"))?
            {
                Some(Event::Byte(source, byte)) => {
                    let show = match source {
                        Source::Host => true,
                        Source::Guest(epoch) if self.attached == Some(epoch) => true,
                        Source::Guest(epoch) => {
                            self.remember(epoch, byte);
                            false
                        }
                    };
                    if !show {
                        continue;
                    }
                    if self.source != Some(source) {
                        self.text.write(out, &visible)?;
                        self.text.finish(out)?;
                        visible.clear();
                        self.source = Some(source);
                    }
                    visible.push(byte);
                }
                Some(Event::Begin(epoch)) => {
                    self.replay(epoch);
                }
                Some(Event::Attach(epoch)) => {
                    self.attached = Some(epoch);
                    self.text.write(out, &visible)?;
                    self.text.finish(out)?;
                    visible.clear();
                    if let Some(mut replay) = self.logs.remove(&epoch) {
                        self.text.write(out, replay.make_contiguous())?;
                    }
                    self.source = Some(Source::Guest(epoch));
                }
                Some(Event::Detach) => {
                    self.attached = None;
                }
                None => {}
            }
        }
        self.text.write(out, &visible)?;
        out.flush()?;
        Ok(())
    }
    fn remember(&mut self, epoch: u32, byte: u8) {
        let replay = self.replay(epoch);
        if replay.len() == REPLAY {
            replay.pop_front();
        }
        replay.push_back(byte);
    }
    fn replay(&mut self, epoch: u32) -> &mut VecDeque<u8> {
        if self.logs.len() == REPLAY_EPOCHS && !self.logs.contains_key(&epoch) {
            self.logs.pop_first();
        }
        self.logs.entry(epoch).or_default()
    }
}

#[derive(Default)]
struct Text {
    pending: Vec<u8>,
}
impl Text {
    fn write(&mut self, out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(bytes);
        let mut remaining = self.pending.as_slice();
        while !remaining.is_empty() {
            let Err(error) = std::str::from_utf8(remaining) else {
                out.write_all(remaining)?;
                remaining = &[];
                break;
            };
            let (valid, invalid) = remaining.split_at(error.valid_up_to());
            out.write_all(valid)?;
            let Some(length) = error.error_len() else {
                remaining = invalid;
                break;
            };
            out.write_all("\u{fffd}".as_bytes())?;
            remaining = &invalid[length..];
        }
        let left = remaining.len();
        self.pending.drain(..self.pending.len() - left);
        Ok(())
    }
    fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        if !self.pending.is_empty() {
            out.write_all("\u{fffd}".as_bytes())?;
            self.pending.clear();
        }
        out.flush()
    }
}

#[cfg(unix)]
mod mode {
    //! POSIX terminal mode.
    //! Ownership/Lock Ordering: one foreground terminal.
    //! Invariants: restore the exact original settings at session exit.
    use anyhow::{Result, ensure};
    use std::process::{Command, Stdio};
    pub struct Mode(String);
    impl Mode {
        pub fn raw() -> Result<Self> {
            let saved = Command::new("stty")
                .arg("-g")
                .stdin(Stdio::inherit())
                .output()?;
            ensure!(saved.status.success(), "read terminal settings with stty");
            let mode = Self(String::from_utf8(saved.stdout)?.trim().to_owned());
            ensure!(
                Command::new("stty")
                    .args(["raw", "-echo", "opost"])
                    .status()?
                    .success(),
                "set terminal raw mode"
            );
            Ok(mode)
        }
    }
    impl Drop for Mode {
        fn drop(&mut self) {
            let _ = Command::new("stty").arg(&self.0).status();
        }
    }
}
#[cfg(windows)]
mod mode {
    //! Native console input mode.
    //! Ownership/Lock Ordering: one foreground terminal.
    //! Invariants: disable local editing and restore the exact original mode.
    use anyhow::Result;
    use std::{ffi::c_void, io};
    type Handle = *mut c_void;
    const STD_INPUT: u32 = (-10i32) as u32;
    const LINE_ECHO_PROCESSED: u32 = 0x07;
    const VT_INPUT: u32 = 0x0200;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(id: u32) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
    }
    pub struct Mode(Handle, u32);
    impl Mode {
        pub fn raw() -> Result<Self> {
            let mut saved = 0;
            // SAFETY: process-owned standard handle; the mode output is a local u32.
            let handle = unsafe { GetStdHandle(STD_INPUT) };
            // SAFETY: GetConsoleMode validates the standard handle and writes one u32.
            if unsafe { GetConsoleMode(handle, &mut saved) } == 0 {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: the preceding call established a live console input handle.
            if unsafe { SetConsoleMode(handle, (saved & !LINE_ECHO_PROCESSED) | VT_INPUT) } == 0 {
                return Err(io::Error::last_os_error().into());
            }
            Ok(Self(handle, saved))
        }
    }
    impl Drop for Mode {
        fn drop(&mut self) {
            // SAFETY: the terminal session holds the original process handle and mode.
            unsafe {
                SetConsoleMode(self.0, self.1);
            }
        }
    }
}
