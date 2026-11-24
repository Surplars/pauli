//! Route a named, otherwise idle device interrupt between two Linux vCPUs.
//! UART output cannot satisfy this test: the event and counters both belong to the RTC.
use super::sys::{self, Result, call};
use crate::rtc_evidence::{Snapshot, Time, one_alarm_read};
use core::{
    ffi::CStr,
    fmt::{self, Write},
};

const IOCTL: usize = 29;
const WRITE: usize = 64;
const PPOLL: usize = 73;
const RTC_RD_TIME: usize = 0x8024_7009;
const RTC_WKALM_SET: usize = 0x4028_700f;
#[repr(C)]
struct Alarm {
    enabled: u8,
    pending: u8,
    reserved: [u8; 2],
    time: Time,
}
const _: () = assert!(size_of::<Time>() == 36 && size_of::<Alarm>() == 40);
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

struct Text {
    bytes: [u8; 128],
    len: usize,
}
impl Write for Text {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        self.bytes
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}
impl Text {
    fn new(args: fmt::Arguments<'_>) -> Result<Self> {
        let mut text = Self {
            bytes: [0; 128],
            len: 0,
        };
        text.write_fmt(args).map_err(|_| ())?;
        Ok(text)
    }
    fn path(&self) -> Result<&CStr> {
        CStr::from_bytes_until_nul(&self.bytes).map_err(|_| ())
    }
}

fn at<T>(result: Result<T>, stage: &'static [u8]) -> Result<T> {
    result.inspect_err(|_| sys::marker(stage))
}

pub fn check() -> Result<()> {
    at(
        sys::mount(c"proc", c"/proc"),
        b"[probe] RTC error: mount proc\n",
    )?;
    at(sys::mount_devices(), b"[probe] RTC error: mount devtmpfs\n")?;
    let fd = at(
        sys::open(c"/dev/rtc0", 0x800),
        b"[probe] RTC error: open rtc0\n",
    )?;
    let irq = at(snapshot(), b"[probe] RTC error: discover IRQ\n")?.irq;
    for cpu in [1, 0] {
        at(route(irq, cpu), b"[probe] RTC error: affinity\n")?;
        let before = at(snapshot(), b"[probe] RTC error: baseline\n")?;
        at(
            sys::check(before.irq == irq),
            b"[probe] RTC error: IRQ identity changed\n",
        )?;
        at(alarm(fd), b"[probe] RTC error: alarm event\n")?;
        let after = at(snapshot(), b"[probe] RTC error: counter read\n")?;
        let [a, b] = before.counts;
        let [c, d] = after.counts;
        let line = Text::new(format_args!(
            "[probe] RTC IRQ irq={irq} cpu={cpu} before={a},{b} after={c},{d}\n"
        ))?;
        sys::marker(&line.bytes[..line.len]);
        at(
            sys::check(before.delivered(after, cpu)),
            b"[probe] RTC error: wrong CPU or no IRQ\n",
        )?;
    }
    sys::close(fd);
    Ok(())
}

fn snapshot() -> Result<Snapshot> {
    let fd = sys::open(c"/proc/interrupts", 0)?;
    let mut buf = [0; 8192];
    let mut len = 0;
    // proc seqfiles may split reads. Never parse an incomplete table.
    while len < buf.len() {
        let n = sys::read(fd, &mut buf[len..])?;
        if n == 0 {
            break;
        }
        len += n;
    }
    sys::close(fd);
    sys::check(len < buf.len())?;
    Snapshot::parse(&buf[..len]).ok_or(())
}

fn route(irq: u32, cpu: usize) -> Result<()> {
    let request = Text::new(format_args!("/proc/irq/{irq}/smp_affinity_list"))?;
    let effective = Text::new(format_args!("/proc/irq/{irq}/effective_affinity_list"))?;
    let dest = if cpu == 0 { b"0\n" } else { b"1\n" };
    let fd = sys::open(request.path()?, 1)?;
    // SAFETY: procfs copies these two live bytes, not a guest-supplied pointer.
    let n = unsafe { call(WRITE, [fd, dest.as_ptr() as usize, dest.len(), 0, 0, 0]) };
    sys::close(fd);
    sys::check(n == dest.len() as isize)?;
    let fd = sys::open(effective.path()?, 0)?;
    let mut value = [0; 16];
    let n = sys::read(fd, &mut value)?;
    sys::close(fd);
    sys::check(value.get(..n) == Some(dest.as_slice()))
}

fn alarm(fd: usize) -> Result<()> {
    let mut now = Time::default();
    // SAFETY: RTC_RD_TIME writes one live 36-byte Linux rtc_time.
    sys::value(unsafe { call(IOCTL, [fd, RTC_RD_TIME, &raw mut now as usize, 0, 0, 0]) })?;
    let alarm = Alarm {
        enabled: 1,
        pending: 0,
        reserved: [0; 2],
        time: now.alarm(),
    };
    // SAFETY: RTC_WKALM_SET reads a complete rtc_wkalrm; it never sets the clock.
    sys::value(unsafe {
        call(
            IOCTL,
            [fd, RTC_WKALM_SET, &raw const alarm as usize, 0, 0, 0],
        )
    })?;
    let mut p = PollFd {
        fd: fd as i32,
        events: 1,
        revents: 0,
    };
    let mut timeout = [5usize, 0]; // Kernel timespec. No dead IRQ can park PID1 forever.
    // SAFETY: one live pollfd, writable timeout, no signal mask.
    sys::check(
        unsafe {
            call(
                PPOLL,
                [
                    &raw mut p as usize,
                    1,
                    timeout.as_mut_ptr() as usize,
                    0,
                    8,
                    0,
                ],
            )
        } == 1
            && p.revents == 1,
    )?;
    let mut bytes = [0; size_of::<usize>()];
    let n = sys::read(fd, &mut bytes)?;
    sys::check(one_alarm_read(n, bytes))
}
