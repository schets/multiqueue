use std::error::Error;
use std::mem;
use std::sync::{Mutex, TryLockError};
use std::sync::atomic::{AtomicUsize, Ordering};

use alloc;
use atomicsignal::AtomicSignal;
use maybe_acquire::{MAYBE_ACQUIRE, maybe_acquire_fence};

struct ToFree {
    mem: *mut u8,
    num_param: usize,
    freer: unsafe fn(*mut u8, usize),
}

/// This is a unique token representing a subscriber to the multiqueue
pub struct MemToken {
    token: u64,
    epoch: AtomicUsize,
}

struct MemoryManagerInner {
    tokens: Vec<*const MemToken>,
    tofree: Vec<ToFree>,
}

pub struct MemoryManager {
    mem_manager: Mutex<MemoryManagerInner>,
    wait_to_free: Mutex<Vec<ToFree>>,
    epoch: AtomicUsize,
    signal: AtomicSignal,
}

impl ToFree {
    pub fn new<T>(val: *mut T, num: usize) -> ToFree {
        unsafe fn do_free<F>(pt: *mut u8, num: usize) {
            let to_free: *mut F = mem::transmute(pt);
            alloc::deallocate(to_free, num);
        }
        ToFree {
            mem: val as *mut u8,
            num_param: num,
            freer: do_free::<T>,
        }
    }

    pub fn delete(self) {
        unsafe { (self.freer)(self.mem, self.num_param) }
    }
}

impl MemoryManagerInner {
    pub fn new() -> MemoryManagerInner {
        MemoryManagerInner {
            tokens: Vec::new(),
            tofree: Vec::new(),
        }
    }

    pub fn free(&mut self, pt: *mut u8, num: usize, freer: fn(*mut u8, usize)) -> bool {
        self.tofree.push(ToFree {
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

impl MemoryManager {
    pub fn new() -> MemoryManager {
        MemoryManager {
            mem_manager: Mutex::new(MemoryManagerInner::new()),
            wait_to_free: Mutex::new(Vec::new()),
            epoch: AtomicUsize::new(0),
            signal: AtomicSignal::new(),
        }
    }

    pub fn start_free(&self) {
        match self.wait_to_free.try_lock() {
            Err(val) => {
                match val {
                    TryLockError::Poisoned(err) => {
                        panic!("Couldn't acquire MemoryManager in multiqueue because: {}",
                               err.description())
                    }
                    _ => (),
                }
            }
            Ok(mut elemvec) => {
                self.do_start_free(&mut elemvec);
            }
        }
    }

    #[cold]
    fn do_start_free(&self, elemvec: &mut Vec<ToFree>) -> bool {
        match self.mem_manager.try_lock() {
            Err(val) => {
                match val {
                    TryLockError::Poisoned(err) => {
                        panic!("Couldn't acquire MemoryManager in multiqueue because: {}",
                               err.description())
                    }
                    _ => (),
                }
                false
            }
            Ok(mut inner) => {
                let mut newv = Vec::new();
                mem::swap(&mut newv, elemvec);
                inner.add_freeable(newv);
                let cur_epoch = self.epoch.load(Ordering::Relaxed);
                self.epoch.store(cur_epoch.wrapping_add(1), Ordering::Relaxed);
                self.signal.clear_start_free(Ordering::Relaxed);
                self.signal.set_epoch(Ordering::Release);
                true
            }
        }
    }

    pub fn free<T>(&self, pt: *mut T, num: usize) {
        let mut elemvec = self.wait_to_free.lock().unwrap();
        elemvec.push(ToFree::new(pt, num));
        if elemvec.len() > 20 {
            if !self.do_start_free(&mut elemvec) {
                self.signal.set_start_free(Ordering::Release);
            }
        }
    }
}
