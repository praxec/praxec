//! Seeded decision: should this executor call FAIL instead of returning output?
//! Drives the harness to exercise failure-handling paths.

use crate::prng::Prng;

pub struct FailureInjector {
    /// 0..=100 — percent of calls that should fail.
    rate: u8,
    rng: std::sync::Mutex<Prng>,
}

impl FailureInjector {
    pub fn new(seed: u64, rate: u8) -> Self {
        Self {
            rate,
            rng: std::sync::Mutex::new(Prng::new(seed)),
        }
    }
    /// True iff this call should be injected as a failure.
    pub fn should_fail(&self) -> bool {
        if self.rate == 0 {
            return false;
        }
        (self.rng.lock().expect("injector rng lock").below(100) as u8) < self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_rate_never_fails() {
        let f = FailureInjector::new(1, 0);
        for _ in 0..100 {
            assert!(!f.should_fail());
        }
    }

    #[test]
    fn full_rate_always_fails() {
        let f = FailureInjector::new(1, 100);
        for _ in 0..100 {
            assert!(f.should_fail());
        }
    }

    #[test]
    fn deterministic_for_seed() {
        let a = FailureInjector::new(7, 50);
        let b = FailureInjector::new(7, 50);
        let sa: Vec<bool> = (0..50).map(|_| a.should_fail()).collect();
        let sb: Vec<bool> = (0..50).map(|_| b.should_fail()).collect();
        assert_eq!(sa, sb);
    }
}
