//! Resident inbox-to-EIP sampling. Count window traffic, not simulated CSR cycles.
use crate::aia::{file_state, imsic::WORDS, registers::MSI_FILE_SIZE, virtualized::VirtualAia};
use core::hint::black_box;
use std::time::Instant;

#[derive(Default)]
struct Counts {
    opens: usize,
    closes: usize,
    sets: usize,
}

struct Window<'a>(&'a mut Counts);

impl<'a> Window<'a> {
    fn open(counts: &'a mut Counts) -> Self {
        counts.opens += 1;
        Self(counts)
    }
}

impl Drop for Window<'_> {
    fn drop(&mut self) {
        self.0.closes += 1;
    }
}

impl file_state::Registers for Window<'_> {
    fn read(&mut self, _: usize) -> usize {
        unreachable!("merge only sets pending bits")
    }
    fn write(&mut self, _: usize, _: usize) {
        unreachable!("merge must not overwrite pending bits")
    }
    fn set(&mut self, select: usize, bits: usize) {
        black_box((select, bits));
        self.0.sets += 1;
    }
}

fn merge(pending: [u64; WORDS], counts: &mut Counts) {
    file_state::merge(pauli_config::ImsicIds::Ids255, pending, || {
        Window::open(counts)
    });
}

fn sample<const PERIOD: usize>(label: &str, ids: &[usize]) {
    const OPS: usize = 1 << 18;
    let mut chip = VirtualAia::new(2);
    let resident = chip.take_file(1);
    let mut samples = [0u64; 9];
    let mut counts = Counts::default();
    for round in 0..2 + samples.len() {
        counts = Counts::default();
        let start = Instant::now();
        for i in 0..OPS {
            let chip = black_box(&mut chip);
            if i % PERIOD == 0 {
                for &id in ids {
                    chip.msi_access(MSI_FILE_SIZE, Some(id));
                }
            }
            merge(chip.take_pending(1), black_box(&mut counts));
        }
        let ns = start.elapsed().as_nanos() as u64;
        assert_eq!(counts.opens, counts.closes);
        // Each injected ID in these fixtures belongs to a different EIP word.
        assert_eq!(counts.sets, OPS / PERIOD * ids.len());
        if round >= 2 {
            samples[round - 2] = ns;
        }
    }
    // A producer may race the final save. Its inbox bit must survive the put.
    chip.msi_access(MSI_FILE_SIZE, Some(255));
    chip.put_file(1, resident);
    assert_eq!(chip.take_file(1).take_pending(), [0, 0, 0, 1 << 63]);
    samples.sort_unstable();
    let ns = |i: usize| samples[i] as f64 / OPS as f64;
    std::println!(
        "IMSIC {label}: n={OPS} open={} close={} eip_set={} host ns/op (no CSR) p25={:.3} median={:.3} p75={:.3}",
        counts.opens,
        counts.closes,
        counts.sets,
        ns(2),
        ns(4),
        ns(6)
    );
}

#[test]
#[ignore = "host cost sample; run explicitly in release mode"]
fn sample_imsic_cost() {
    sample::<1>("empty", &[]);
    sample::<128>("one-in-128", &[13]);
    sample::<1>("one", &[13]);
    sample::<1>("cross-word", &[13, 197]);
}
