//! Host-only scored events, in input order. The caller exports only after clean QEMU exit.
//! Mode changes and unscored restart probes never enter a plateau.
use super::{Mode, Sample, timeline};
use anyhow::{Context, Result};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

pub(super) struct Population {
    pub reserved: usize,
    pub waiting: usize,
}

struct Plateau {
    peers: Population,
    epoch: u32,
    owner: u8,
    hz: u64,
    events: Vec<(Mode, Sample)>,
}

#[derive(Default)]
pub(in crate::smoke) struct Capture(Vec<Plateau>);

impl Capture {
    pub(super) fn record(
        &mut self,
        peers: Population,
        epoch: u32,
        owner: u8,
        hz: u64,
        events: Vec<(Mode, Sample)>,
    ) -> Result<()> {
        for mode in [Mode::Wait, Mode::Busy] {
            let samples: Vec<_> = events
                .iter()
                .filter(|(m, _)| *m == mode)
                .map(|(_, s)| *s)
                .collect();
            timeline::report(&samples, epoch, owner, mode.name(), hz)?;
        }
        self.0.push(Plateau {
            peers,
            epoch,
            owner,
            hz,
            events,
        });
        Ok(())
    }

    pub(in crate::smoke) fn save(&self, path: &Path) -> Result<()> {
        let file = File::create_new(path).with_context(|| {
            format!(
                "create new IRQ sample CSV {} (never overwrites)",
                path.display()
            )
        })?;
        let mut out = BufWriter::new(file);
        self.write_csv(&mut out)?;
        out.flush()?;
        Ok(())
    }

    fn write_csv(&self, out: &mut impl Write) -> std::io::Result<()> {
        writeln!(
            out,
            "phase,reserved,waiting,epoch,owner,timebase_hz,mode,input,sample,rise_ticks,owner_ticks,trap_ticks,ack_path,kick_send_ticks,ssip_ack_ticks,kick_return_ticks"
        )?;
        for (index, p) in self.0.iter().enumerate() {
            for (mode, sample) in &p.events {
                let [input, seq, rise, observed, trap] = sample.raw();
                write!(
                    out,
                    "{},{},{},{},{},{},{},{input},{seq},{rise},{observed},{trap}",
                    index + 1,
                    p.peers.reserved,
                    p.peers.waiting,
                    p.epoch,
                    p.owner,
                    p.hz,
                    mode.name()
                )?;
                match sample.ack {
                    Some(a) => writeln!(
                        out,
                        ",{},{},{},{}",
                        a.path.name(),
                        a.send,
                        a.ticks,
                        a.returned
                    )?,
                    None => writeln!(out, ",,,,")?,
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_retains_absolute_ticks_and_order_on_repeated_plateaus() -> Result<()> {
        use timeline::{Ack, AckPath};
        let sample = |input, seq, tick| Sample::new(input, seq, tick, tick + 2, tick + 3).unwrap();
        let phase = |events| Plateau {
            peers: Population {
                reserved: 8,
                waiting: 4,
            },
            epoch: 8,
            owner: 0,
            hz: 10_000_000,
            events,
        };
        let capture = Capture(vec![
            phase(vec![
                (Mode::Wait, sample(2, 1, 100)),
                (
                    Mode::Busy,
                    sample(4, 2, 105).with_ack(Some(Ack {
                        path: AckPath::GuestExit,
                        send: 105,
                        ticks: 107,
                        returned: 110,
                    }))?,
                ),
            ]),
            phase(vec![(Mode::Wait, sample(6, 3, u64::MAX - 3))]),
        ]);
        let mut csv = Vec::new();
        capture.write_csv(&mut csv)?;
        let lines: Vec<_> = std::str::from_utf8(&csv)?.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].ends_with(",ack_path,kick_send_ticks,ssip_ack_ticks,kick_return_ticks"));
        assert!(lines.iter().all(|line| line.split(',').count() == 16));
        assert_eq!(
            &lines[1..],
            [
                "1,8,4,8,0,10000000,wait,2,1,100,102,103,,,,",
                "1,8,4,8,0,10000000,busy,4,2,105,107,108,guest-exit,105,107,110",
                "2,8,4,8,0,10000000,wait,6,3,18446744073709551612,18446744073709551614,18446744073709551615,,,,",
            ]
        );
        Ok(())
    }

    #[test]
    fn existing_output_is_not_truncated() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut scratch = crate::media::Scratch::new(root)?;
        let (path, mut keep) = scratch.file("samples.csv")?;
        keep.write_all(b"original")?;
        drop(keep);
        assert!(Capture::default().save(&path).is_err());
        assert_eq!(std::fs::read(path)?, b"original");
        Ok(())
    }
}
