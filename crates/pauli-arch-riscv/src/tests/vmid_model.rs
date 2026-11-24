//! Production algorithm acceptance, using private host fixtures.
//! Ownership/Lock Ordering: each test owns its state; no actual CSR access.
//! Invariants: retain the original protocol assertions and rejection cases.
use crate::{remote::fence, vmid::policy};
use fence::FenceBatch;
use policy::{Cache, INVALIDATE, Key, Plan, Statistics, TAGGED, TAGS, VS_INVALIDATE};

pub(super) struct Hart {
    pub(super) cache: Cache,
    pub(super) gstage: [Option<Key>; TAGS + 1],
    pub(super) vstage: [Option<(Key, u8, u64)>; TAGS + 1],
    pub(super) gwalks: [Option<Key>; TAGS + 1],
    pub(super) vwalks: [Option<Key>; TAGS + 1],
    pub(super) stats: Statistics,
}
impl Hart {
    pub(super) fn new(tags: usize) -> Self {
        Self {
            cache: Cache::new(tags),
            gstage: [None; TAGS + 1],
            vstage: [None; TAGS + 1],
            gwalks: [None; TAGS + 1],
            vwalks: [None; TAGS + 1],
            stats: Statistics::default(),
        }
    }
    pub(super) fn enter(&mut self, key: Key, version: u64, fresh: bool) -> Plan {
        self.enter_cpu(key, 0, version, fresh)
    }
    pub(super) fn enter_cpu(&mut self, key: Key, cpu: u8, version: u64, fresh: bool) -> Plan {
        let plan = self.cache.select(key, cpu, fresh);
        let tag = usize::from(plan.tag);
        // Mirror entry.S's legal flag values, including its x0/non-x0
        // distinction. Global GVMA need not invalidate VS-only entries.
        match plan.flags() {
            0 => {
                self.gstage.fill(None);
                self.vstage[0] = None;
                self.gwalks.fill(None);
                self.vwalks[0] = None;
            }
            TAGGED => {
                assert!(!plan.flush && !plan.vs_flush && tag > 0);
            }
            flags if flags == TAGGED | INVALIDATE => {
                self.gstage[tag] = None;
                self.vstage[tag] = None;
                self.gwalks[tag] = None;
                self.vwalks[tag] = None;
            }
            flags if flags == TAGGED | VS_INVALIDATE => {
                assert!(!plan.flush && plan.vs_flush && tag > 0);
                self.vstage[tag] = None;
                self.vwalks[tag] = None;
            }
            flags => panic!("invalid assembly plan {flags}"),
        }
        if let Some(cached) = self.gstage[tag] {
            assert_eq!(cached, key, "foreign GPA/HPA mapping survived tag reuse");
        }
        if let Some(cached) = self.vstage[tag] {
            assert_eq!(cached, (key, cpu, version), "stale VS translation survived");
        }
        self.gstage[tag] = Some(key);
        self.vstage[tag] = Some((key, cpu, version));
        self.gwalks[tag] = Some(key);
        self.vwalks[tag] = Some(key);
        if plan.flags() != TAGGED {
            self.stats.entry_flushes += 1;
        } else {
            self.stats.warm_entries += 1;
        }
        self.stats.vmid = plan.tag;
        // Tagged return restores HS's reserved zero without invalidation.
        if tag == 0 {
            self.gstage.fill(None);
            self.vstage[0] = None;
            self.gwalks.fill(None);
            self.vwalks[0] = None;
        }
        plan
    }
    pub(super) fn synchronize(&mut self, target: Key, instruction: bool) {
        self.synchronize_batch(target, FenceBatch::single(instruction));
    }
    pub(super) fn synchronize_batch(&mut self, target: Key, batch: FenceBatch) {
        if batch.instruction {
            self.stats.remote_icache += 1;
        }
        if batch.translation {
            if let Some(tag) = self.cache.find(target) {
                self.vstage[usize::from(tag)] = None;
                self.vwalks[usize::from(tag)] = None;
                self.stats.remote_tlb += 1;
            } else {
                assert!(!self.vstage.iter().flatten().any(|entry| entry.0 == target));
                assert!(!self.gstage.iter().flatten().any(|entry| *entry == target));
                assert!(!self.gwalks.iter().flatten().any(|entry| *entry == target));
                assert!(!self.vwalks.iter().flatten().any(|entry| *entry == target));
            }
        }
        self.stats.remote_syncs += 1;
    }
    pub(super) fn retire(&mut self, target: Key) {
        if let Some(tag) = self.cache.find(target) {
            let tag = usize::from(tag);
            self.gstage[tag] = None;
            self.vstage[tag] = None;
            self.gwalks[tag] = None;
            self.vwalks[tag] = None;
            self.cache.forget(target);
        }
        self.stats.retirements += 1;
    }
    pub(super) fn has_walk(&self, target: Key) -> bool {
        self.gwalks
            .iter()
            .chain(self.vwalks.iter())
            .flatten()
            .any(|key| *key == target)
    }
}
pub(super) fn key(epoch: u32) -> Key {
    Key {
        epoch,
        root: (8usize << 60) | (epoch as usize * 4 + 0x80000),
    }
}
