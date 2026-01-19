use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub trait FlagAtomic {
    fn is_true(&self) -> bool;
    fn set_bool(&self, value: bool);
}

impl FlagAtomic for Arc<AtomicBool> {
    fn is_true(&self) -> bool {
        self.load(Ordering::Relaxed)
    }

    fn set_bool(&self, value: bool) {
        self.store(value, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flag_atomic_initial_false() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!flag.is_true());
    }

    #[test]
    fn test_flag_atomic_initial_true() {
        let flag = Arc::new(AtomicBool::new(true));
        assert!(flag.is_true());
    }

    #[test]
    fn test_flag_atomic_set_true() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!flag.is_true());

        flag.set_bool(true);
        assert!(flag.is_true());
    }

    #[test]
    fn test_flag_atomic_set_false() {
        let flag = Arc::new(AtomicBool::new(true));
        assert!(flag.is_true());

        flag.set_bool(false);
        assert!(!flag.is_true());
    }

    #[test]
    fn test_flag_atomic_toggle() {
        let flag = Arc::new(AtomicBool::new(false));

        flag.set_bool(true);
        assert!(flag.is_true());

        flag.set_bool(false);
        assert!(!flag.is_true());

        flag.set_bool(true);
        assert!(flag.is_true());
    }

    #[test]
    fn test_flag_atomic_clone_shared_state() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();

        flag.set_bool(true);
        assert!(flag_clone.is_true());

        flag_clone.set_bool(false);
        assert!(!flag.is_true());
    }
}
