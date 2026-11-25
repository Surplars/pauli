use super::*;
use crate::arch::MAX_CPUS;
use core::{
    ptr,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

fn words(slot: &OwnerStats) -> [&AtomicU64; 16] {
    [
        &slot.hart_id,
        &slot.vm_exits,
        &slot.wfi_exits,
        &slot.sbi_timer_exits,
        &slot.sstc_timer_exits,
        &slot.mmio_exits,
        &slot.other_exits,
        &slot.vtimer_irqs,
        &slot.ext_irqs,
        &slot.last_scause,
        &slot.last_sepc,
        &slot.last_stval,
        &slot.idle_wfi,
        &slot.mail_sends,
        &slot.mail_recvs,
        &slot.reserved,
    ]
}

#[test]
fn debugger_wire_offsets_and_unregistered_owner_values() {
    let stats = HsStats::new();
    let header = [
        &stats.magic,
        &stats.version,
        &stats.header_bytes,
        &stats.owner_stride,
        &stats.host_cpus,
        &stats.sstc_owners,
        &stats.exclusive_owners,
        &stats.reserved,
    ];
    assert_eq!(
        header.map(|word| word.load(Relaxed)),
        [HS_STATS_MAGIC, 2, 64, 128, 0, 0, 0, 0]
    );
    let base = ptr::from_ref(&stats) as usize;
    for (i, word) in header.into_iter().enumerate() {
        assert_eq!(ptr::from_ref(word) as usize - base, i * 8);
    }
    for (i, slot) in stats.owners.iter().enumerate() {
        for (j, word) in words(slot).into_iter().enumerate() {
            assert_eq!(ptr::from_ref(word) as usize - base, 64 + i * 128 + j * 8);
            assert_eq!(word.load(Relaxed), if j == 0 { u64::MAX } else { 0 });
        }
    }
    assert_eq!(size_of::<HsStats>(), 8256);
    stats.init_host(2, 2);
    stats.register_owner(0, 0, false);
    stats.register_owner(1, 19, true);
    stats.register_owner(2, 2, true);
    stats.register_owner(usize::MAX, 3, true);
    assert!(StatsTracer::with_stats(&stats, 2).is_none());
    assert!(StatsTracer::with_stats(&stats, usize::MAX).is_none());
    assert_eq!(stats.host_cpus.load(Relaxed), 2);
    assert_eq!(stats.sstc_owners.load(Relaxed), 2);
    assert_eq!(stats.exclusive_owners.load(Relaxed), 2);
    assert_eq!(stats.owners[0].hart_id.load(Relaxed), 0);
    assert_eq!(stats.owners[1].hart_id.load(Relaxed), 19);
    assert_eq!(stats.owners[2].hart_id.load(Relaxed), UNREGISTERED_HART);
}

#[test]
fn independent_harts_record_exit_buckets_snapshots_and_completion_counts() {
    let stats = HsStats::new();
    let upper = MAX_CPUS - 1;
    stats.init_host(MAX_CPUS, 1 << upper);
    std::thread::scope(|s| {
        for (owner, physical, turns) in [(0, 8, 2), (upper, 42, 3)] {
            let stats = &stats;
            s.spawn(move || {
                stats.register_owner(owner, physical, true);
                let mut tracer = StatsTracer::with_stats(stats, owner).unwrap();
                for turn in 0..turns {
                    let exit = VcpuExit {
                        kind: ExitKind::Resume,
                        cause: (1usize << (usize::BITS - 1)) | physical,
                        pc: 0x8000_0000 + turn * 4,
                        value: physical * 2,
                        native_event: false,
                        deadline_reached: false,
                    };
                    for source in [
                        ExitSource::Wfi,
                        ExitSource::SbiTimer,
                        ExitSource::SstcTimer,
                        ExitSource::Mmio,
                        ExitSource::External,
                        ExitSource::Other,
                        ExitSource::Software,
                        ExitSource::SupervisorCall,
                        ExitSource::Timer,
                    ] {
                        tracer.on_guest_exit(source, &exit, source == ExitSource::Timer);
                    }
                    tracer.on_vtimer_irq(); // Parked software timer, with no associated VM exit.
                    tracer.on_idle_wfi();
                    tracer.on_mail(true);
                    tracer.on_mail(false);
                    tracer.on_mail(false);
                }
            });
        }
    });
    assert_eq!(stats.sstc_owners.load(Relaxed), 1 | (1 << upper));
    assert_eq!(stats.exclusive_owners.load(Relaxed), 1 << upper);
    for (owner, hart, n) in [(0, 8u64, 2u64), (upper, 42, 3)] {
        assert_eq!(
            words(&stats.owners[owner]).map(|word| word.load(Relaxed)),
            [
                hart,
                9 * n,
                n,
                n,
                n,
                n,
                4 * n,
                2 * n,
                n,
                (1u64 << (usize::BITS - 1)) | hart,
                0x8000_0000 + (n - 1) * 4,
                hart * 2,
                n,
                n,
                2 * n,
                0,
            ]
        );
    }
    for slot in &stats.owners[1..upper] {
        assert_eq!(slot.hart_id.load(Relaxed), UNREGISTERED_HART);
        assert!(words(slot)[1..].iter().all(|word| word.load(Relaxed) == 0));
    }
}
