use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use maybe_acquire::{MAYBE_ACQUIRE, maybe_acquire_fence};

struct ToFree {
    mem: *mut u8,
    num_param: usize,
    freer: fn(*mut u8, usize),
}

/// This is a unique token representing a subscriber to the multiqueue
pub struct MemToken {
    token: u64,
    epoch: AtomicUsize,
}

pub struct MemoryManagerInner {
    tokens: Vec<*const MemToken>,
    tofree: Vec<ToFree>,
    internal_epoch: usize,
}

pub struct MemoryManager {
    mem_manager: Mutex<MemoryManagerInner>,
    wait_to_free: Mutex<Vec<ToFree>>,
    epoch: AtomicUsize,
}

impl ToFree {
   pub fn delete(self) {
        unsafe { (self.freer)(self.mem, self.num_param) }
    }
}

impl MemoryManagerInner {

    pub fn free(&mut self, pt: *mut u8, num: usize, freer: fn(*mut u8, usize)) -> bool {
        self.tofree.push(ToFree{
            mem: pt,
            num_param: num,
            freer: freer,
        });
        self.tofree.len() > 10
    }

    pub fn try_freeing(&mut self, at: usize) -> bool {
        for token_ptr in &self.tokens {
            unsafe {
                let token = &**token_ptr;
                let epoch = token.epoch.load(MAYBE_ACQUIRE);
                if epoch != at {
                   return false;
                }
            }
        }
        maybe_acquire_fence();
        for val in self.tofree.drain(..) {
            val.delete();
        }
        true
    }

    pub fn add_freeable(&mut self, vec: Vec<ToFree>) {
        self.tofree = vec;
    }
}