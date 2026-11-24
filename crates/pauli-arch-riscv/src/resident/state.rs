//! Owner-local assembly protocol and accounting; no hardware or VM policy.
//! Ownership/Lock Ordering: one physical owner, no locks or hardware access.
//! Invariants: load/resume/put is idempotent at cleanup; no state survives a foreign context.
//! Protocol: Empty --load--> Loaded --resume--> Loaded --put--> Empty.
pub const FULL: usize = 0;
pub const LOAD: usize = 1;
pub const RESUME: usize = 2;

#[derive(Clone, Copy, Default)]
pub struct Statistics {
    pub loads: u64,
    pub resumes: u64,
    pub puts: u64,
    pub fp_loads: u64,
    pub fp_puts: u64,
}
impl Statistics {
    pub fn merge(&mut self, other: Self) {
        self.loads += other.loads;
        self.resumes += other.resumes;
        self.puts += other.puts;
        self.fp_loads += other.fp_loads;
        self.fp_puts += other.fp_puts;
    }
}

#[derive(Default)]
pub struct State {
    pub loaded: bool,
    pub fp_loaded: bool,
    pub statistics: Statistics,
}
impl State {
    pub fn enter(&mut self) -> usize {
        if self.loaded {
            self.statistics.resumes += 1;
            RESUME
        } else {
            self.loaded = true;
            self.statistics.loads += 1;
            LOAD
        }
    }
    pub fn load_fp(&mut self, enabled: bool) -> bool {
        if enabled && !self.fp_loaded {
            self.fp_loaded = true;
            self.statistics.fp_loads += 1;
            true
        } else {
            false
        }
    }
    pub fn put_fp(&mut self) -> bool {
        if !self.fp_loaded {
            return false;
        }
        self.fp_loaded = false;
        self.statistics.fp_puts += 1;
        true
    }
    /// Idempotent so control, error and lexical cleanup share one put path.
    pub fn put(&mut self) -> bool {
        if !self.loaded {
            return false;
        }
        self.loaded = false;
        self.statistics.puts += 1;
        true
    }
}
