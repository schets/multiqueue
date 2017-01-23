
use std::sync::atomic::{Ordering, fence};

#[cfg(any(target_arch = "x64", target_arch = "x64_64", target_arch = "aarch64"))]
mod theimpl {
    use std::sync::atomic::{Ordering, fence};
    pub const MAYBE_ACQUIRE: Ordering = Ordering::Relaxed;

    #[inline(always)]
    pub fn maybe_acquire_fence() {
        fence(Ordering::Acquire)
    }
}

#[cfg(not(any(target_arch = "x64", target_arch = "x64_64", target_arch = "aarch64")))]
mod theimpl {
    use std::sync::atomic::Ordering;
    pub const MAYBE_ACQUIRE: Ordering = Ordering::Acquire;

    #[inline(always)]
    pub fn maybe_acquire_fence() {}
}

pub const MAYBE_ACQUIRE: Ordering = theimpl::MAYBE_ACQUIRE;

#[inline(always)]
pub fn maybe_acquire_fence() {
    theimpl::maybe_acquire_fence()
}
