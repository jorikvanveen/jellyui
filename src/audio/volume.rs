use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

/// Application volume, shared by the UI and each replacement audio worker.
#[derive(Clone)]
pub struct Volume(Arc<AtomicU32>);

impl Default for Volume {
    fn default() -> Self {
        Self(Arc::new(AtomicU32::new(1f32.to_bits())))
    }
}

impl Volume {
    pub fn level(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, level: f32) {
        if level.is_finite() {
            self.0
                .store(level.clamp(0., 1.).to_bits(), Ordering::Relaxed);
        }
    }
}
