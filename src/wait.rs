use std::sync::atomic::Ordering::Relaxed;
use std::thread::yield_now;

use countedindex::{CountedIndex, past};

extern crate parking_lot;

const DEFAULT_YIELD_SPINS: usize = 50;
const DEFAULT_TRY_SPINS: usize = 1000;

/// This trait determines how readers wait on the empty queue
pub trait Wait {
    /// Causes the reader to block until the queue is available. Is passed
    /// the index which the readers are waiting on and a reference to the
    /// index which the writers are using
    fn wait(&self, usize, &CountedIndex);

    /// Called by writers to awaken waiting readers
    fn notify(&self);

    /// Returns whether writers need to call notify
    /// Optimized the various BusyWait variants
    fn needs_notify(&self) -> bool {
        false
    }
}

/// Waiting
#[derive(Copy, Clone)]
pub struct BusyWait {}

#[derive(Copy, Clone)]
pub struct YieldingWait {
    spins_first: usize,
    spins_yield: usize,
}

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
    pub fn new() -> YieldingWait {
        YieldingWait::with_spins(DEFAULT_TRY_SPINS, DEFAULT_YIELD_SPINS)
    }

    pub fn with_spins(spins_first: usize, spins_yield: usize) -> YieldingWait {
        YieldingWait {
            spins_first: spins_first,
            spins_yield: spins_yield,
        }
    }
}

impl BlockingWait {
    pub fn new() -> BlockingWait {
        BlockingWait::with_spins(DEFAULT_TRY_SPINS, DEFAULT_YIELD_SPINS)
    }

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
    fn wait(&self, seq: usize, w_pos: &CountedIndex) {
        loop {
            let cur_pos = w_pos.load_count(Relaxed);
            if cur_pos == seq || past(seq, cur_pos).1 {
                return;
            }
        }
    }

    fn notify(&self) {
        // Nothing here since the waiter just blocks on the writer flag
    }
}

impl Wait for YieldingWait {
    #[cold]
    fn wait(&self, seq: usize, w_pos: &CountedIndex) {
        for _ in 0..self.spins_first {
            let cur_pos = w_pos.load_count(Relaxed);
            if cur_pos == seq || past(seq, cur_pos).1 {
                return;
            }
        }
        loop {
            for _ in 0..self.spins_yield {
                let cur_pos = w_pos.load_count(Relaxed);
                if cur_pos == seq || past(seq, cur_pos).1 {
                    return;
                }
            }
            yield_now();
        }
    }

    fn notify(&self) {
        // Nothing here since the waiter just blocks on the writer flag
    }
}

impl Wait for BlockingWait {
    #[cold]
    fn wait(&self, seq: usize, w_pos: &CountedIndex) {
        for _ in 0..self.spins_first {
            let cur_pos = w_pos.load_count(Relaxed);
            if cur_pos == seq || past(seq, cur_pos).1 {
                return;
            }
        }
        for _ in 0..self.spins_yield {
            let cur_pos = w_pos.load_count(Relaxed);
            if cur_pos == seq || past(seq, cur_pos).1 {
                return;
            }
            yield_now();
        }

        loop {
            {
                let mut lock = self.lock.lock();
                let cur_pos = w_pos.load_count(Relaxed);
                if cur_pos == seq || past(seq, cur_pos).1 {
                    return;
                }
                self.condvar.wait(&mut lock);
            }
            let cur_pos = w_pos.load_count(Relaxed);
            if cur_pos == seq || past(seq, cur_pos).1 {
                return;
            }

        }
    }

    #[allow(unused_variables)]
    fn notify(&self) {
        // I don't try and do any flag tricks here to avoid the notify
        // since they would require a store-load fence or an rmw operation.
        // on top of potentially doing the mutex and condition variable.
        // The fast path here is pretty fast anyways
        let lock = self.lock.lock();
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
                let this_reader = reader.add_reader().into_single().unwrap();
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
