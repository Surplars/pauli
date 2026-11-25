//! Fixed-width little-endian boot manifest ABI; never Rust layout.
//! Ownership/Lock Ordering: exclusive caller ownership; no locks in this module.
//! Invariants: decode rejects noncanonical padding/flags; encode validates all ranges.

use super::*;

// Linux keeps both previously reserved fields zero. Old decoders reject
// firmware records rather than mapping them with Linux RAM permissions.
const RX_SHIFT: u32 = 16;
const RO_SHIFT: u32 = 32;
const PAGE_COUNT_MASK: u64 = u16::MAX as u64;

impl BootConfig {
    pub fn encode(&self) -> Result<[u8; WIRE_SIZE], Error> {
        self.validate()?;
        let mut out = [0; WIRE_SIZE];
        out[..8].copy_from_slice(MAGIC);
        for (i, value) in [
            self.template_base,
            self.template_size,
            self.dtb_size,
            u64::from(self.management) | (u64::from(!self.smoke_payload) << 1),
            self.disks.iter().flatten().count() as u64,
            self.count() as u64,
            0,
        ]
        .into_iter()
        .enumerate()
        {
            put(&mut out, 8 + i * 8, value);
        }
        for (index, vm) in self.vms.iter().flatten().enumerate() {
            let start = HEADER + index * RECORD;
            out[start..start + NAME_MAX].copy_from_slice(&vm.name);
            let (cpus, affinity) = placement_words(vm);
            for (i, value) in [
                vm.guest_base,
                vm.host_base,
                vm.memory_size,
                vm.dtb_offset,
                vm.dtb_size,
                vm.quantum_us,
                u64::from(vm.autostart)
                    | (u64::from(vm.disk.is_some()) << 1)
                    | (u64::from(vm.writable) << 2)
                    | (u64::from(vm.net_peer.is_some()) << 3)
                    | (u64::from(vm.net_peer.unwrap_or(0)) << 4)
                    | (u64::from(vm.net_uplink) << 6)
                    | (u64::from(vm.stop_on_reset) << 7)
                    | (u64::from(vm.disk.unwrap_or(0)) << 8)
                    | (u64::from(vm.aia) << 10)
                    | (u64::from(vm.exclusive_cpus) << 11)
                    | (u64::from(vm.host_stats) << 12)
                    | vm.firmware.map_or(0, |f| {
                        (u64::from(f.rx_pages) << RX_SHIFT) | (u64::from(f.ro_pages) << RO_SHIFT)
                    }),
                cpus,
                affinity,
                vm.image.kernel_source,
                vm.image.kernel_size,
                vm.image.kernel_memory_size,
                vm.image.initrd_source,
                vm.image.initrd_size,
            ]
            .into_iter()
            .enumerate()
            {
                put(&mut out, start + NAME_MAX + i * 8, value);
            }
        }
        for (index, disk) in self.disks.iter().flatten().enumerate() {
            let start = DISKS_OFFSET + index * DISK_RECORD;
            put(&mut out, start, disk.sectors);
            put(
                &mut out,
                start + 8,
                u64::from(disk.writable) | (u64::from(disk.managed) << 1),
            );
        }
        Ok(out)
    }
    pub fn decode(raw: &[u8; WIRE_SIZE]) -> Result<Self, Error> {
        if &raw[..8] != MAGIC {
            return Err(Error::Version);
        }
        let count = get(raw, 48);
        if !(1..=MAX_BOOT_VMS as u64).contains(&count) {
            return Err(Error::Count);
        }
        let flags = get(raw, 32);
        if flags & !3 != 0 || get(raw, 56) != 0 {
            return Err(Error::Reserved);
        }
        let mut vms = [None; MAX_BOOT_VMS];
        for (index, entry) in vms.iter_mut().enumerate() {
            let start = HEADER + index * RECORD;
            if index >= count as usize {
                if raw[start..start + RECORD].iter().any(|b| *b != 0) {
                    return Err(Error::Reserved);
                }
                continue;
            }
            *entry = Some(decode_vm(raw, start)?);
        }
        let disk_count = get(raw, 40);
        if disk_count > MAX_DISKS as u64 {
            return Err(Error::Disk);
        }
        let mut disks = [None; MAX_DISKS];
        for (index, entry) in disks.iter_mut().enumerate() {
            let start = DISKS_OFFSET + index * DISK_RECORD;
            if index >= disk_count as usize {
                if raw[start..start + DISK_RECORD]
                    .iter()
                    .any(|byte| *byte != 0)
                {
                    return Err(Error::Reserved);
                }
                continue;
            }
            let flags = get(raw, start + 8);
            if flags & !3 != 0 {
                return Err(Error::Reserved);
            }
            *entry = Some(DiskConfig {
                sectors: get(raw, start),
                writable: flags & 1 != 0,
                managed: flags & 2 != 0,
            });
        }
        let config = Self {
            template_base: get(raw, 8),
            template_size: get(raw, 16),
            dtb_size: get(raw, 24),
            management: flags & 1 != 0,
            smoke_payload: flags & 2 == 0,
            disks,
            vms,
        };
        config.validate()?;
        Ok(config)
    }
}
fn put(out: &mut [u8], at: usize, value: u64) {
    out[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
fn get(raw: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&raw[at..at + 8]); // Fixed ABI offsets, not guest-selected indices.
    u64::from_le_bytes(word)
}

fn decode_vm(raw: &[u8; WIRE_SIZE], start: usize) -> Result<VmConfig, Error> {
    let mut name = [0; NAME_MAX];
    name.copy_from_slice(&raw[start..start + NAME_MAX]);
    let end = name.iter().position(|b| *b == 0).unwrap_or(NAME_MAX);
    let text = core::str::from_utf8(&name[..end]).map_err(|_| Error::Name)?;
    let mut vm = VmConfig::new(text)?;
    if vm.name != name {
        return Err(Error::Name);
    }
    vm.guest_base = get(raw, start + 24);
    vm.host_base = get(raw, start + 32);
    vm.memory_size = get(raw, start + 40);
    vm.dtb_offset = get(raw, start + 48);
    vm.dtb_size = get(raw, start + 56);
    vm.quantum_us = get(raw, start + 64);
    let flags = get(raw, start + 72);
    let rx_pages = ((flags >> RX_SHIFT) & PAGE_COUNT_MASK) as u16;
    let ro_pages = ((flags >> RO_SHIFT) & PAGE_COUNT_MASK) as u16;
    if flags & !(0x1fff | (PAGE_COUNT_MASK << RX_SHIFT) | (PAGE_COUNT_MASK << RO_SHIFT)) != 0
        || (rx_pages == 0 && ro_pages != 0)
        || (flags & 2 == 0 && flags & 0x300 != 0)
        || (flags & 8 == 0 && flags & 0x30 != 0)
    {
        return Err(Error::Reserved);
    }
    vm.autostart = flags & 1 != 0;
    vm.disk = (flags & 2 != 0).then_some(((flags >> 8) & 3) as u8);
    vm.writable = flags & 4 != 0;
    vm.net_peer = (flags & 8 != 0).then_some(((flags >> 4) & 3) as u8);
    vm.net_uplink = flags & 0x40 != 0;
    vm.stop_on_reset = flags & 0x80 != 0;
    vm.aia = flags & 0x400 != 0;
    vm.exclusive_cpus = flags & 0x800 != 0;
    vm.host_stats = flags & 0x1000 != 0;
    vm.firmware = (rx_pages != 0).then_some(FirmwareLayout { rx_pages, ro_pages });
    (vm.vcpus, vm.cpu_affinity) = read_placement(get(raw, start + 80), get(raw, start + 88))?;
    vm.image = VmImage {
        kernel_source: get(raw, start + 96),
        kernel_size: get(raw, start + 104),
        kernel_memory_size: get(raw, start + 112),
        initrd_source: get(raw, start + 120),
        initrd_size: get(raw, start + 128),
    };
    Ok(vm)
}

const LEGACY_PINS: usize = 4;
const OWNER_BITS: usize = 2;
const OWNER_MASK: u32 = (1 << OWNER_BITS) - 1;
const WIDE_PINS: u64 = 3 << 32;
const NATIVE_TAG: u64 = 4 << 56;
const NATIVE_BITS: usize = 6;
const HALF: usize = 8;
const HALF_BITS: usize = HALF * NATIVE_BITS;
const _: () = assert!(
    MAX_HOST_CPUS as usize == 1 << NATIVE_BITS
        && MAX_GUEST_VCPUS as usize == 2 * HALF
        && MAX_GUEST_VCPUS as usize * OWNER_BITS == u32::BITS as usize
);

/// Preserve every legacy encoding. Only a long list that actually needs high
/// owners uses two 48-bit halves; old decoders reject its unknown tag.
fn placement_words(vm: &VmConfig) -> (u64, u64) {
    let count = u64::from(vm.vcpus);
    let word = match vm.cpu_affinity {
        CpuAffinity::Automatic => 0,
        CpuAffinity::Workers => 2 << 32,
        CpuAffinity::Physical(pins) if usize::from(vm.vcpus) <= LEGACY_PINS => {
            (1 << 32) | u64::from(u32::from_le_bytes([pins[0], pins[1], pins[2], pins[3]]))
        }
        CpuAffinity::Physical(pins) if needs_native(&pins) => {
            let pack = |half: &[u8]| {
                half.iter().enumerate().fold(0u64, |word, (i, &owner)| {
                    word | (u64::from(owner) << (i * NATIVE_BITS))
                })
            };
            return (
                count | (pack(&pins[HALF..]) << 8),
                NATIVE_TAG | pack(&pins[..HALF]),
            );
        }
        CpuAffinity::Physical(pins) => pins
            .iter()
            .enumerate()
            .fold(WIDE_PINS, |word, (i, &owner)| {
                word | (u64::from(owner) << (i * OWNER_BITS))
            }),
    };
    (count, word)
}
fn needs_native(pins: &[u8]) -> bool {
    pins.iter().any(|&id| u32::from(id) > OWNER_MASK)
}
fn read_placement(cpus: u64, word: u64) -> Result<(u8, CpuAffinity), Error> {
    if word >> 56 != NATIVE_TAG >> 56 {
        let count = u8::try_from(cpus).map_err(|_| Error::Vcpus)?;
        return Ok((count, read_affinity(word, count)?));
    }
    let count = cpus as u8;
    if usize::from(count) <= LEGACY_PINS
        || word >> HALF_BITS != NATIVE_TAG >> HALF_BITS
        || cpus >> (8 + HALF_BITS) != 0
    {
        return Err(Error::Reserved);
    }
    let pins = core::array::from_fn(|i| {
        let half = if i < HALF { word } else { cpus >> 8 };
        ((half >> (i % HALF * NATIVE_BITS)) & ((1 << NATIVE_BITS) - 1)) as u8
    });
    if !needs_native(&pins) {
        return Err(Error::Reserved); // Same pins must never have two wire encodings.
    }
    Ok((count, CpuAffinity::Physical(pins)))
}
fn read_affinity(word: u64, count: u8) -> Result<CpuAffinity, Error> {
    let low = word as u32;
    match word {
        0 => Ok(CpuAffinity::Automatic),
        w if w == 2 << 32 => Ok(CpuAffinity::Workers),
        w if w >> 32 == 1 && usize::from(count) <= LEGACY_PINS => {
            let mut pins = [0; MAX_GUEST_VCPUS as usize];
            pins[..LEGACY_PINS].copy_from_slice(&low.to_le_bytes());
            Ok(CpuAffinity::Physical(pins))
        }
        w if w >> 32 == WIDE_PINS >> 32 && usize::from(count) > LEGACY_PINS => {
            Ok(CpuAffinity::Physical(core::array::from_fn(|i| {
                ((low >> (i * OWNER_BITS)) & OWNER_MASK) as u8
            })))
        }
        _ => Err(Error::Reserved),
    }
}
