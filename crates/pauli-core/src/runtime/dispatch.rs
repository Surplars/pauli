//! Global round-robin over small placement groups, with bounded per-group IRQ masking.
//! Ownership/Lock Ordering: each worker scans its own mail; pins never span another VM.
//! Invariants: process mail before guest entry; cursor names a lane, not an execution grant.
use super::*;
use deadline::{TimerTurn, Visit};
use placement::{GROUP, LANES, STRIDE};

enum First {
    Done(bool),
    More {
        groups: usize,
        cursor: usize,
        head: Placement,
    },
}

impl<A: HvArch, B: Board> VmRuntime<A, B> {
    pub fn dispatch<R: Tracer>(&self, local: &mut Dispatcher<A::Timer, R>) -> Result<bool> {
        let owner = A::Smp::cpu_id();
        let mut parked = deadline::ParkedTimers::for_turn(local.timer_turn);
        let first = masked::<A, _>(|| {
            // A new group is published before its owner is kicked. Never take
            // the group bound first and then acknowledge the kick that announced it.
            A::Smp::acknowledge_kick();
            let groups = self.manager.placement_groups();
            if groups == 1 {
                let set = self.scan_mail(0, owner, local, &mut parked)?;
                if self.try_timer_peer(owner, local, &parked)? {
                    return Ok(First::Done(true));
                }
                let sole = set.count() == 1;
                let ran = self.run_group(0, owner, set, local.cursor % LANES, local, &parked, sole)?
                    || self.run_deferred(owner, local, &parked, sole)?;
                return Ok(First::Done(ran));
            }
            let cursor = if local.cursor / LANES < groups {
                local.cursor
            } else {
                0
            };
            let head = self.scan_mail(cursor / LANES, owner, local, &mut parked)?;
            Ok(First::More {
                groups,
                cursor,
                head,
            })
        })?;
        let (groups, cursor, mut head) = match first {
            First::Done(ran) => return Ok(ran),
            First::More {
                groups,
                cursor,
                head,
            } => (groups, cursor, head),
        };
        let first = cursor / LANES;
        let others = (first + 1..groups).chain(0..first);
        let mut ready_lanes = head.count();
        for g in others.clone() {
            ready_lanes += masked::<A, _>(|| self.scan_mail(g, owner, local, &mut parked))?.count();
        }
        let sole = ready_lanes == 1;
        if matches!(local.timer_turn, TimerTurn::Offered(_))
            && masked::<A, _>(|| self.try_timer_peer(owner, local, &parked))?
        {
            return Ok(true);
        }
        let tail = head.take_before(cursor % LANES);
        if masked::<A, _>(|| self.run_group(first, owner, head, 0, local, &parked, sole))? {
            return Ok(true);
        }
        for g in others {
            let set = self.manager.placed_on(g, owner);
            if masked::<A, _>(|| self.run_group(g, owner, set, 0, local, &parked, sole))? {
                return Ok(true);
            }
        }
        masked::<A, _>(|| {
            Ok(self.run_group(first, owner, tail, 0, local, &parked, sole)?
                || self.run_deferred(owner, local, &parked, sole)?)
        })
    }
    /// If a regular turn surrendered the hart to a timed peer, offer that
    /// waiter one admission without moving the ordinary rotation. The offer is
    /// consumed even if it fails. Only matching sleepers may share its budget.
    fn try_timer_peer<R: Tracer>(
        &self,
        owner: usize,
        local: &mut Dispatcher<A::Timer, R>,
        parked: &deadline::ParkedTimers,
    ) -> Result<bool> {
        let TimerTurn::Offered(offer) = local.timer_turn else {
            return Ok(false);
        };
        local.timer_turn = TimerTurn::Normal;
        let lane = offer.target.lane;
        self.run_exact(
            lane,
            owner,
            local,
            Visit::Due {
                offer,
                peer: parked.for_peer(lane).at,
                sibling: parked.sibling,
            },
            false,
        )
    }
    // An expedited lane has already run, even if it would also be next in
    // ordinary rotation. Only retry it here, when no other guest could run.
    fn run_deferred<R: Tracer>(
        &self,
        owner: usize,
        local: &mut Dispatcher<A::Timer, R>,
        parked: &deadline::ParkedTimers,
        sole: bool,
    ) -> Result<bool> {
        let TimerTurn::Served { lane, .. } = local.timer_turn else {
            return Ok(false);
        };
        local.timer_turn = TimerTurn::Normal;
        if !self.run_exact(lane, owner, local, Visit::Regular(parked.for_peer(lane)), sole)? {
            return Ok(false);
        }
        local.cursor = lane + 1;
        Ok(true)
    }
    fn run_exact<R: Tracer>(
        &self,
        lane: usize,
        owner: usize,
        local: &mut Dispatcher<A::Timer, R>,
        visit: Visit,
        sole: bool,
    ) -> Result<bool> {
        let Some(vm) = self.manager.available(lane / STRIDE) else {
            return Ok(false);
        };
        let Some(slot) = vm.cpu(lane % STRIDE).filter(|s| s.owner == owner) else {
            return Ok(false);
        };
        if !vm.lifecycle.can_enter() {
            return Ok(false);
        }
        self.process(&vm, slot, local, visit, sole)
    }
    fn run_group<R: Tracer>(
        &self,
        group: usize,
        owner: usize,
        mut set: Placement,
        mut cursor: usize,
        local: &mut Dispatcher<A::Timer, R>,
        parked: &deadline::ParkedTimers,
        sole: bool,
    ) -> Result<bool> {
        while let Some(flat) = set.pop_from(cursor) {
            cursor = (flat + 1) % LANES;
            let id = flat / STRIDE;
            let Some(vm) = self.manager.available(group * GROUP + id) else {
                continue;
            };
            if !vm.lifecycle.can_enter() {
                set.remove_vm(id);
                continue;
            }
            // A stale mask can name a recycled slot: recheck the physical owner under its pin.
            let Some(slot) = vm.cpu(flat % STRIDE).filter(|cpu| cpu.owner == owner) else {
                continue;
            };
            let lane = group * LANES + flat;
            if matches!(local.timer_turn, TimerTurn::Served { lane: past, epoch }
                if lane == past && vm.epoch() == epoch)
            {
                continue;
            }
            let peer = parked.for_peer(lane);
            if self.process(&vm, slot, local, Visit::Regular(peer), sole)? {
                local.cursor = lane + 1;
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn scan_mail<R: Tracer>(
        &self,
        group: usize,
        owner: usize,
        local: &mut Dispatcher<A::Timer, R>,
        parked: &mut deadline::ParkedTimers,
    ) -> Result<Placement> {
        let mut pending = self.manager.placed_on(group, owner);
        let mut candidates = pending;
        while let Some((id, mut lanes)) = pending.pop_vm() {
            let Some(vm) = self.manager.available(group * GROUP + id) else {
                continue;
            };
            let state = vm.lifecycle.state();
            if state != State::Ready {
                candidates.remove_vm(id);
                if state != State::Stopping {
                    continue;
                }
            }
            while lanes != 0 {
                let index = lanes.trailing_zeros() as usize;
                lanes &= lanes - 1;
                let Some(slot) = vm.cpu(index).filter(|slot| slot.owner == owner) else {
                    continue;
                };
                if state == State::Stopping || slot.mailbox.lock().pending() {
                    self.process(&vm, slot, local, Visit::Mail, false)?;
                }
                if state == State::Ready {
                    // Sample after mail: an IPI may already have woken this lane.
                    // The physical owner is the sole runner; no peer context lock.
                    parked.observe(
                        vm.slot * STRIDE + index,
                        slot.parked_timer.load(Ordering::Relaxed),
                        A::Timer::now,
                    );
                }
            }
        }
        Ok(candidates)
    }
}
