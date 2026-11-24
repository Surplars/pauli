//! Optional Ssaia bank transfer; register access is the only hardware seam.
//! Ownership/Lock Ordering: exclusive physical owner, IRQs masked through restore.
//! Invariants: restore HVIEN before independent VSIE; no host priority leaks.
use super::{GuestState, NEUTRAL_CONTROL};

#[derive(Clone, Copy)]
pub enum Register {
    Enable,
    Control,
    Select,
    Priority1,
    Priority2,
    Vsie,
}
pub trait Registers {
    fn read(&mut self, register: Register) -> usize;
    fn write(&mut self, register: Register, value: usize);
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostState {
    enable: usize,
    control: usize,
    guest: GuestState,
    priority1: usize,
    priority2: usize,
    vsie: usize,
}
pub fn snapshot(regs: &mut impl Registers) -> GuestState {
    GuestState {
        select: regs.read(Register::Select),
    }
}
pub fn set_external(regs: &mut impl Registers, external: bool) {
    regs.write(Register::Control, NEUTRAL_CONTROL | usize::from(external));
}
pub fn load(
    regs: &mut impl Registers,
    available: bool,
    guest: GuestState,
    external: bool,
) -> Option<HostState> {
    if !available {
        return None;
    }
    let host = HostState {
        enable: regs.read(Register::Enable),
        control: regs.read(Register::Control),
        guest: snapshot(regs),
        priority1: regs.read(Register::Priority1),
        priority2: regs.read(Register::Priority2),
        vsie: regs.read(Register::Vsie),
    };
    regs.write(Register::Enable, 0);
    set_external(regs, external);
    regs.write(Register::Select, guest.select);
    regs.write(Register::Priority1, 0);
    regs.write(Register::Priority2, 0);
    Some(host)
}
impl HostState {
    pub fn restore(self, regs: &mut impl Registers) {
        regs.write(Register::Select, self.guest.select);
        regs.write(Register::Priority1, self.priority1);
        regs.write(Register::Priority2, self.priority2);
        regs.write(Register::Enable, self.enable);
        regs.write(Register::Control, self.control);
        // HVIEN 0->1 makes independent enables unspecified; VSIE is last.
        regs.write(Register::Vsie, self.vsie);
    }
}
