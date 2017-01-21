
use std::sync::atomic::Ordering;
// I'm aware of silly issues with dependency tracking and things like
// f = load_consume(...); *a[f - f]; that isn't actually consume
// This project uses it exclusively for things like b = *a, c = *b

#[cfg(any(target_arch = "x64",
          target_arch = "x64_64",
          target_arch = "aarch64",
          target_arch = "arm"))]
mod CanConsume {
    use std::sync::atomic::Ordering;
    pub const Consume: Ordering = Ordering::Relaxed;
}

#[cfg(not(any(target_arch = "x64",
              target_arch = "x64_64",
              target_arch = "aarch64",
              target_arch = "arm")))]
mod CanConsume {
    use std::sync::atomic::Ordering;
    pub const Consume: Ordering = Ordering::Acquire;
}

pub const Consume: Ordering = CanConsume::Consume;
