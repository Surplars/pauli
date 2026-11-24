//! Native wait after an empty dispatch; never changes a running vCPU's slice.
//! Firmware-only snapshots pin live instances and take one short lock at a time before masking IRQs.
use super::*;

const FAST_POLL_HZ: u64 = 10_000;
const MONITOR_POLL_HZ: u64 = 1_000;

#[derive(Clone, Copy)]
enum Wait {
    Poll,
    QuietMonitor,
    Until {
        at: u64,
        fast_io: bool,
        #[cfg(feature = "selftest")]
        guest_timer: bool,
    },
    Interrupt,
}

impl<A: HvArch, B: Board> VmRuntime<A, B> {
    /// Remote devices and polled UARTs still need the fast service tick. Shorten
    /// it only for a parked compare on this hart; no device or context lock here.
    fn firmware_monitor_poll(&self, owner: usize) -> Wait {
        if !self.manager.has_assignments(owner) {
            return Wait::Poll;
        }
        let mut at = u64::MAX;
        for group in 0..self.manager.placement_groups() {
            let mut local = self.manager.placed_on(group, owner);
            while let Some((id, _)) = local.pop_vm() {
                let Some(vm) = self.manager.available(group * placement::GROUP + id) else {
                    continue;
                };
                // Firmware has one lane. Recheck its owner under the pin in case
                // a clone slot was reused after the placement snapshot.
                if vm.lifecycle.can_enter()
                    && let Some(slot) = vm.cpu(0).filter(|slot| slot.owner == owner)
                {
                    at = at.min(slot.parked_timer.load(Ordering::Relaxed));
                }
            }
        }
        if at == u64::MAX {
            Wait::Poll
        } else {
            Wait::Until {
                at,
                fast_io: true,
                #[cfg(feature = "selftest")]
                guest_timer: true,
            }
        }
    }

    fn firmware_wait(&self, owner: usize, manager: bool) -> Wait {
        if !self.firmware_only {
            return Wait::Poll;
        }
        if manager && (self.board.info().host_cpus != 1 || !B::IRQ_DRIVEN_CONSOLE) {
            return self.firmware_monitor_poll(owner);
        }
        let (mut guest, mut alarm, mut fast_io) = (None::<u64>, None::<u64>, false);
        for vm in self.manager.vms() {
            let Some(slot) = vm.cpu(0).filter(|slot| slot.owner == owner) else {
                continue;
            };
            match vm.lifecycle.state() {
                State::Ready => {}
                State::Stopping => return Wait::Poll, // Owners or devices still owe retirement.
                // Joined clocks are frozen. Only the monitor's undrained TX
                // survives here; a subsequent start publishes before kicking.
                State::Stopped | State::Faulted => {
                    if manager {
                        fast_io |= vm.devices.io.lock().console.has_output();
                    }
                    continue;
                }
            }
            // The management hart services UART, RTC and board work; workers
            // need only their own timers and published device/lifecycle kicks.
            if manager {
                let io = vm.devices.io.lock();
                fast_io |= io.needs_fast_idle_service();
                if let Some(at) = io.rtc.irq_deadline() {
                    alarm = Some(alarm.map_or(at, |old| old.min(at)));
                }
            }
            let data = slot.data.lock();
            let Some(cpu) = data
                .context
                .as_ref()
                .filter(|_| data.state == CpuState::Waiting)
            else {
                return Wait::Poll;
            };
            if let Some(at) = cpu.wait_deadline() {
                guest = Some(guest.map_or(at, |old| old.min(at)));
            }
        }
        match guest.into_iter().chain(alarm).min() {
            Some(at) => Wait::Until {
                at,
                fast_io,
                #[cfg(feature = "selftest")]
                guest_timer: guest == Some(at),
            },
            None if manager && !fast_io => Wait::QuietMonitor,
            None if manager => Wait::Poll,
            None => Wait::Interrupt,
        }
    }

    pub fn idle<R: Tracer>(&self, local: &mut Dispatcher<A::Timer, R>) -> Result<()> {
        let info = self.board.info();
        let owner = A::Smp::cpu_id();
        let manager = owner == info.boot_cpu;
        let plan = self.firmware_wait(owner, manager);
        masked::<A, _>(|| {
            // The native ISR may already have cleared its interrupt and queued
            // the byte since the last monitor pass. Do not sleep on that work.
            if manager
                && matches!(plan, Wait::QuietMonitor | Wait::Until { .. })
                && self.board.console_rx_queued()
            {
                return Ok(());
            }
            #[cfg(feature = "selftest")]
            let mut guest_compare = None;
            // Recheck placement under the IRQ mask so a concurrent start kick is
            // either visible in the directory or still pending when we reach WFI.
            let timed = !matches!(plan, Wait::Interrupt)
                && (manager || self.manager.has_assignments(owner));
            if timed {
                let now = A::Timer::now();
                let poll = |hz: u64| now.saturating_add((info.timebase_hz / hz).max(1));
                let deadline = match plan {
                    Wait::Until { at, .. } if at <= now => return Ok(()), // Service before WFI.
                    // Keep the monitor responsive; a UART backlog needs the shorter bound.
                    // Neither maintenance interval may postpone a known earlier deadline.
                    Wait::Until { at, fast_io, .. } if manager => at.min(poll(if fast_io {
                        FAST_POLL_HZ
                    } else {
                        MONITOR_POLL_HZ
                    })),
                    Wait::Until { at, .. } => at,
                    Wait::QuietMonitor => poll(MONITOR_POLL_HZ),
                    _ => poll(FAST_POLL_HZ),
                };
                // SAFETY: exclusive native timer, all guest banks put.
                unsafe { local.timer.arm(deadline)? };
                #[cfg(feature = "selftest")]
                {
                    if matches!(plan, Wait::Until { at, guest_timer: true, .. } if at == deadline) {
                        guest_compare = Some(deadline);
                    }
                }
            } else {
                // SAFETY: this worker has no local deadline; pending kicks remain enabled.
                unsafe { local.timer.disarm()? };
            }
            // SAFETY: masked local IRQs, no live guest bank; a pending IPI survives to WFI.
            unsafe {
                #[cfg(not(feature = "selftest"))]
                A::Smp::idle();
                #[cfg(feature = "selftest")]
                self.observe_idle(owner, !timed, guest_compare);
            }
            Ok(())
        })
    }
}
