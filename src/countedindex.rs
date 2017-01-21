use std::sync::atomic::{AtomicUsize, Ordering};

pub fn get_valid_wrap(val: u32) -> u32 {
    let max = 1 << 30;
    if val >= max {
        max
    } else {
        val.next_power_of_two()
    }
}

fn validate_wrap(val: u32) {
    assert!(val.is_power_of_two(),
            "Multiqueue error - non power-of-two size received");
    assert!(val <= (1 << 30),
            "Multiqueue error - too large size received");
    assert!(val > 0, "Multiqueue error - zero size received");
}

// A queue entry will never ever have this value as an initial valid flag
// Since the upper 2/66 bits will never be set
pub const INITIAL_QUEUE_FLAG: usize = ::std::usize::MAX;

pub struct CountedIndex {
    val: AtomicUsize,
    mask: usize,
}

pub struct Transaction<'a> {
    ptr: &'a AtomicUsize,
    loaded_vals: usize,
    mask: usize,
    lord: Ordering,
}

impl CountedIndex {
    pub fn new(wrap: u32) -> CountedIndex {
        validate_wrap(wrap);
        CountedIndex {
            val: AtomicUsize::new(0),
            mask: (wrap - 1) as usize,
        }
    }

    pub fn from_usize(val: usize, wrap: u32) -> CountedIndex {
        validate_wrap(wrap);
        CountedIndex {
            val: AtomicUsize::new(val),
            mask: (wrap - 1) as usize,
        }
    }

    pub fn wrap_at(&self) -> u32 {
        self.mask as u32 + 1
    }

    #[inline(always)]
    pub fn load(&self, ord: Ordering) -> u32 {
        (self.val.load(ord) & self.mask) as u32
    }

    #[inline(always)]
    pub fn load_raw(&self, ord: Ordering) -> usize {
        self.val.load(ord)
    }

    #[inline(always)]
    pub fn load_count(&self, ord: Ordering) -> usize {
        self.load_raw(ord)
    }

    #[inline(always)]
    pub fn load_transaction(&self, ord: Ordering) -> Transaction {
        Transaction {
            ptr: &self.val,
            loaded_vals: self.val.load(ord),
            lord: ord,
            mask: self.mask,
        }
    }

    #[inline(always)]
    pub fn get_previous(start: usize, by: u32) -> usize {
        start.wrapping_sub(by as usize)
    }
}

impl<'a> Transaction<'a> {
    /// Loads the index and the expected valid flag
    #[inline(always)]
    pub fn get(&self) -> (isize, usize) {
        ((self.loaded_vals & self.mask) as isize, self.loaded_vals)
    }

    /// Returns true if the values passed in matches the previous wrap-around of the Transaction
    #[inline(always)]
    pub fn matches_previous(&self, val: usize) -> bool {
        let wrap = self.mask.wrapping_add(1);
        self.loaded_vals.wrapping_sub(wrap) == val
    }

    #[inline(always)]
    pub fn commit(self, by: u32, ord: Ordering) -> Option<Transaction<'a>> {
        let store_val = self.loaded_vals.wrapping_add(by as usize);
        match self.ptr.compare_exchange_weak(self.loaded_vals, store_val, ord, self.lord) {
            Ok(_) => None,
            Err(cval) => {
                Some(Transaction {
                    ptr: self.ptr,
                    loaded_vals: cval,
                    lord: self.lord,
                    mask: self.mask,
                })
            }
        }
    }

    #[inline(always)]
    pub fn commit_direct(self, by: u32, ord: Ordering) {
        let store_val = self.loaded_vals.wrapping_add(by as usize);
        self.ptr.store(store_val, ord);
    }
}

unsafe impl Send for CountedIndex {}
unsafe impl Sync for CountedIndex {}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate crossbeam;
    use self::crossbeam::scope;

    use std::sync::atomic::Ordering::*;

    fn test_incr_param(wrap_size: u32, goaround: usize) {
        let mycounted = CountedIndex::new(wrap_size);
        for j in 0..goaround {
            for i in 0..wrap_size as usize {
                let trans = mycounted.load_transaction(Relaxed);
                assert_eq!(i, mycounted.load(Relaxed) as usize);
                assert_eq!(i + (j * wrap_size as usize), mycounted.load_count(Relaxed));
                assert_eq!(i, trans.get().0 as usize);
                trans.commit_direct(1, Release);
            }
        }
        // Is wrap_size * goaround % wrap_size, so trivially zero
        assert_eq!(0, mycounted.load(Relaxed));
        assert_eq!(wrap_size as usize * goaround, mycounted.load_count(Relaxed));
    }

    fn test_incr_param_threaded(wrap_size: u32, goaround: usize, nthread: usize) {
        let mycounted = CountedIndex::new(wrap_size);
        scope(|scope| for _ in 0..nthread {
            scope.spawn(|| for j in 0..goaround {
                for i in 0..wrap_size {
                    let mut trans = mycounted.load_transaction(Relaxed);
                    loop {
                        match trans.commit(1, Release) {
                            Some(new_t) => trans = new_t,
                            None => break,
                        }
                    }
                }
            });
        });
        assert_eq!(0, mycounted.load(Relaxed));
        assert_eq!(wrap_size as usize * goaround * nthread,
                   mycounted.load_count(Relaxed));
    }

    #[test]
    fn test_small() {
        test_incr_param(16, 100);
    }

    #[test]
    fn test_tiny() {
        test_incr_param(1, 100)
    }

    #[test]
    fn test_wrapu16() {
        test_incr_param(1 + ::std::u16::MAX as u32, 2)
    }

    #[test]
    fn test_small_mt() {
        test_incr_param_threaded(16, 1000, 2)
    }

    #[test]
    fn test_tiny_mt() {
        test_incr_param_threaded(1, 10000, 2)
    }

    #[test]
    fn test_wrapu16_mt() {
        test_incr_param_threaded(::std::u16::MAX as u32 + 1, 2, 13)
    }

    #[test]
    fn test_transaction_fail() {
        let mycounted = CountedIndex::new(16);
        let trans = mycounted.load_transaction(Relaxed);
        let trans2 = mycounted.load_transaction(Relaxed);
        trans2.commit_direct(1, Relaxed);
        trans.commit(1, Relaxed).unwrap();
    }
}
