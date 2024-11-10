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
