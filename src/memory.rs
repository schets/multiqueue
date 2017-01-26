use std::mem;
use std::ptr;
use std::sync::Mutex;
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
    epoch: AtomicUsize,
}

struct MemoryManagerInner {
    tokens: Vec<*const MemToken>,
    tofree: Vec<ToFree>,
    epoch: usize,
}

pub struct MemoryManager {
    mem_manager: Mutex<MemoryManagerInner>,
    wait_to_free: Mutex<Vec<ToFree>>,
    epoch: AtomicUsize,
    pub signal: AtomicSignal,
}

impl ToFree {
    pub fn new<T>(val: *mut T, num: usize) -> ToFree {
        unsafe fn do_free<F>(pt: *mut u8, num: usize) {
            let to_free: *mut F = mem::transmute(pt);
            for i in 0..num as isize {
                ptr::read(to_free.offset(i));
            }
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
            epoch: 0,
        }
    }

    pub fn get_token(&mut self, at: usize) -> *const MemToken {
        let token = alloc::allocate(1);
        self.tokens.push(token as *const MemToken);
        unsafe {
            ptr::write(token, MemToken { epoch: AtomicUsize::new(at) });
        }
        token as *const MemToken
    }

    pub fn remove_token(&mut self, token: *const MemToken) {
        self.tokens.retain(|x| *x != token);
    }

    pub fn try_freeing(&mut self, at: usize) -> bool {
        if self.tokens.len() == 0 {
            return false;
        }
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
        self.epoch = at;
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

    pub fn get_token(&self) -> *const MemToken {
        let mut inner = self.mem_manager.lock().unwrap();
        inner.get_token(self.epoch.load(Ordering::Acquire))
    }

    pub fn remove_token(&self, token: *const MemToken) {
        self.update_token(token);
        let mut inner = self.mem_manager.lock().unwrap();
        inner.remove_token(token);
        self.free(token as *mut MemToken, 1);
    }

    #[cold]
    fn start_free(&self, elemvec: &mut Vec<ToFree>) {
        let _ = self.mem_manager.try_lock().map(|mut inner| {
            let cur_epoch = self.epoch.load(Ordering::Relaxed);
            if inner.epoch == cur_epoch {
                let mut newv = Vec::new();
                mem::swap(&mut newv, elemvec);
                inner.add_freeable(newv);
                self.epoch.store(cur_epoch.wrapping_add(1), Ordering::Release);
                self.signal.set_epoch(Ordering::Release);
            }
        });
    }

    #[cold]
    pub fn free<T>(&self, pt: *mut T, num: usize) {
        let mut elemvec = self.wait_to_free.lock().unwrap();
        elemvec.push(ToFree::new(pt, num));
        {
            let _ = self.mem_manager
                .try_lock()
                .map(|mut inner| {
                    let epoch = self.epoch.load(Ordering::SeqCst);
                    if inner.try_freeing(epoch) {
                        self.signal.clear_epoch(Ordering::Release);
                    }
                });
        }
        if elemvec.len() > 20 {
            self.start_free(&mut elemvec);
        }
    }

    #[cold]
    pub fn update_token(&self, val: *const MemToken) {
        unsafe {
            let token = &*val;
            let epoch = self.epoch.load(Ordering::Relaxed);
            let token_e = token.epoch.load(Ordering::Relaxed);
            if token_e != epoch {
                token.epoch.store(epoch, Ordering::Release);
            }
        }
    }
}
