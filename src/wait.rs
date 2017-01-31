//! This module contains the waiting strategies used by the queue
//! when there is no data left. Users should not find themselves
//! directly accessing these except for construction
//! unless a custom Wait is being written.
//!
//! # Examples
//!
//! ```
//! use multiqueue::wait;
//! use multiqueue::multiqueue_with;
//! let _ = multiqueue_with::<usize>(10, wait::BusyWait::new());
//! let _ = multiqueue_with::<usize>(10, wait::YieldingWait::new()); // also see with_spins
//! let _ = multiqueue_with::<usize>(10, wait::BlockingWait::new()); // also see with_spins
//! ```
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::AtomicUsize;
use std::thread::yield_now;

use countedindex::{past, rm_tag};

extern crate parking_lot;

pub const DEFAULT_YIELD_SPINS: usize = 50;
pub const DEFAULT_TRY_SPINS: usize = 1000;

#[inline(always)]
pub fn load_tagless(val: &AtomicUsize) -> usize {
    rm_tag(val.load(Relaxed))
}

#[inline(always)]
pub fn check(seq: usize, at: &AtomicUsize, wc: &AtomicUsize) -> bool {
    let cur_count = load_tagless(at);
    wc.load(Relaxed) == 0 || seq == cur_count || past(seq, cur_count).1
}

/// This is the trait that something implements to allow receivers
/// to block waiting for more data.
pub trait Wait {
    /// Causes the reader to block until the queue is available. Is passed
    /// the queue tag which the readers are waiting on, a reference to the
    /// corresponding AtomicUsize, and a reference to the number of writers
    fn wait(&self, usize, &AtomicUsize, &AtomicUsize);

    /// Called by writers to awaken waiting readers
    fn notify(&self);

    /// Returns whether writers need to call notify
    /// Optimized the various BusyWait variants
    fn needs_notify(&self) -> bool;
}

/// Thus spins in a loop on the queue waiting for a value to be ready
#[derive(Copy, Clone)]
pub struct BusyWait {}

/// This spins on the queue for a few iterations and then starts yielding intermittently
#[derive(Copy, Clone)]
pub struct YieldingWait {
    spins_first: usize,
    spins_yield: usize,
}

/// This tries spinning on the queue for a short while, then yielding, and then blocks
pub struct BlockingWait {
    spins_first: usize,
    spins_yield: usize,
    lock: parking_lot::Mutex<bool>,
    condvar: parking_lot::Condvar,
}

unsafe impl Sync for BusyWait {}
unsafe impl Sync for YieldingWait {}
unsafe impl Sync for BlockingWait {}
unsafe impl Send for BusyWait {}
unsafe impl Send for YieldingWait {}
unsafe impl Send for BlockingWait {}

impl BusyWait {
    pub fn new() -> BusyWait {
        BusyWait {}
    }
}

impl YieldingWait {
    /// Calls with_spins(DEFAULT_TRY_SPINS, DEFAULT_YIELD_SPINS)
    pub fn new() -> YieldingWait {
        YieldingWait::with_spins(DEFAULT_TRY_SPINS, DEFAULT_YIELD_SPINS)
    }

    /// Constructs a YieldingWait that busywaits for spins_first spins
    /// and then yields every spins_yield spins.
    pub fn with_spins(spins_first: usize, spins_yield: usize) -> YieldingWait {
        YieldingWait {
            spins_first: spins_first,
            spins_yield: spins_yield,
        }
    }
}

impl BlockingWait {
    /// Calls with_spins(DEFAULT_TRY_SPINS, DEFAULT_YIELD_SPINS)
    pub fn new() -> BlockingWait {
        BlockingWait::with_spins(DEFAULT_TRY_SPINS, DEFAULT_YIELD_SPINS)
    }

    /// Constructs a YieldingWait that busywaits for spins_first spins
    /// and then yields for spins_yield spins, then blocks on a condition variable.
    pub fn with_spins(spins_first: usize, spins_yield: usize) -> BlockingWait {
        BlockingWait {
            spins_first: spins_first,
            spins_yield: spins_yield,
            lock: parking_lot::Mutex::new(false),
            condvar: parking_lot::Condvar::new(),
        }
    }
}

impl Wait for BusyWait {
    #[cold]
    fn wait(&self, seq: usize, w_pos: &AtomicUsize, wc: &AtomicUsize) {
        loop {
            if check(seq, w_pos, wc) {
                return;
            }
        }
    }

    fn notify(&self) {
        // Nothing here since the waiter just blocks on the writer flag
    }

    fn needs_notify(&self) -> bool {
        false
    }
}

impl Wait for YieldingWait {
    #[cold]
    fn wait(&self, seq: usize, w_pos: &AtomicUsize, wc: &AtomicUsize) {
        for _ in 0..self.spins_first {
            if check(seq, w_pos, wc) {
                return;
            }
        }
        loop {
            yield_now();
            for _ in 0..self.spins_yield {
                if check(seq, w_pos, wc) {
                    return;
                }
            }
        }
    }

    fn notify(&self) {
        // Nothing here since the waiter just blocks on the writer flag
    }

    fn needs_notify(&self) -> bool {
        false
    }
}


impl Wait for BlockingWait {
    #[cold]
    fn wait(&self, seq: usize, w_pos: &AtomicUsize, wc: &AtomicUsize) {
        for _ in 0..self.spins_first {
            if check(seq, w_pos, wc) {
                return;
            }
        }
        for _ in 0..self.spins_yield {
            yield_now();
            if check(seq, w_pos, wc) {
                return;
            }
        }

        loop {
            {
                let mut lock = self.lock.lock();
                if check(seq, w_pos, wc) {
                    return;
                }
                self.condvar.wait(&mut lock);
            }
            if check(seq, w_pos, wc) {
                return;
            }

        }
    }

    fn notify(&self) {
        // I don't try and do any flag tricks here to avoid the notify
        // since they would require a store-load fence or an rmw operation.
        // on top of potentially doing the mutex and condition variable.
        // The fast path here is pretty fast anyways
        let _lock = self.lock.lock();
        self.condvar.notify_all();
    }

    fn needs_notify(&self) -> bool {
        true
    }
}

impl Clone for BlockingWait {
    fn clone(&self) -> BlockingWait {
        BlockingWait::with_spins(self.spins_first, self.spins_yield)
    }
}

#[cfg(test)]
mod test {

    use std::sync::atomic::{AtomicUsize, fence, Ordering};
    use std::thread::yield_now;

    use super::*;
    use multiqueue::multiqueue_with;

    extern crate crossbeam;
    use self::crossbeam::scope;

    #[inline(never)]
    fn waste_some_us(n_us: usize, to: &AtomicUsize) {
        for _ in 0..20 * n_us {
            to.store(0, Ordering::Relaxed);
            fence(Ordering::SeqCst);
        }
    }

    fn mpsc_broadcast<W: Wait + 'static>(senders: usize, receivers: usize, waiter: W) {
        let (writer, reader) = multiqueue_with(4, waiter);
        let num_loop = 1000;
        scope(|scope| {
            for q in 0..senders {
                let cur_writer = writer.clone();
                scope.spawn(move || {
                    let v = AtomicUsize::new(0);
                    'outer: for i in 0..num_loop {
                        for _ in 0..100000000 {
                            if cur_writer.try_send((q, i)).is_ok() {
                                waste_some_us(1, &v); // makes sure readers block
                                continue 'outer;
                            }
                            yield_now();
                        }
                        assert!(false, "Writer could not write");
                    }
                });
            }
            writer.unsubscribe();
            for _ in 0..receivers {
                let this_reader = reader.add_stream().into_single().unwrap();
                scope.spawn(move || {
                    let mut myv = Vec::new();
                    for _ in 0..senders {
                        myv.push(0);
                    }
                    for _ in 0..num_loop * senders {
                        if let Ok(val) = this_reader.recv() {
                            assert_eq!(myv[val.0], val.1);
                            myv[val.0] += 1;
                        } else {
                            panic!("Writer got disconnected early");
                        }
                    }
                    for val in myv {
                        if val != num_loop {
                            panic!("Wrong number of values obtained for this");
                        }
                    }
                    assert!(this_reader.try_recv().is_err());
                });
            }
            reader.unsubscribe();
        });
    }

    fn test_waiter<T: Wait + Clone + 'static>(waiter: T) {
        mpsc_broadcast(2, 2, waiter.clone());
    }

    #[test]
    fn test_busywait() {
        test_waiter(BusyWait::new());
    }

    #[test]
    fn test_yieldwait() {
        test_waiter(YieldingWait::new());
    }

    #[test]
    fn test_blockingwait() {
        test_waiter(BlockingWait::new());
    }

    #[test]
    fn test_blockingwait_nospin() {
        test_waiter(BlockingWait::with_spins(0, 0));
    }

}
