use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_pointer_width="32")]
mod index_data {

    pub type Index = u32;

    // 03th and 31 bit are available
    pub const MAX_WRAP: Index = (1 << 30) - 1;

    pub const MASK_IND: Index = (1 << 31);

}

#[cfg(target_pointer_width="64")]
mod index_data {

    pub type Index = u64;

    /// 2 less than the highest bits
    pub const MAX_WRAP: Index = (1 << 62) - 1;

    pub const MASK_IND: Index = (1 << 63);
}

pub type Index = index_data::Index;

const MASK_IND: usize = index_data::MASK_IND as usize;
const MASK_TAG: usize = MASK_IND - 1;
const MAX_WRAP: Index = index_data::MAX_WRAP;

#[inline(always)]
pub fn past(check: usize, seq: usize) -> (usize, bool) {
    let diff = check.wrapping_sub(seq);
    (diff, diff > MAX_WRAP as usize)
}

#[inline(always)]
pub fn is_tagged(val: usize) -> bool {
    (val & MASK_IND) != 0
}

#[inline(always)]
pub fn rm_tag(val: usize) -> usize {
    val & MASK_TAG
}

pub fn get_valid_wrap(val: Index) -> Index {
    if val >= MAX_WRAP {
        MAX_WRAP
    } else if val == 0 {
        1
    } else {
        val.next_power_of_two()
    }
}

fn validate_wrap(val: Index) {
    assert!(val.is_power_of_two(),
            "Multiqueue error - non power-of-two size received");
    assert!(val <= MAX_WRAP,
            "Multiqueue error - too large size received");
    assert!(val > 0, "Multiqueue error - zero size received");
}


// A queue entry will never ever have this value as an initial valid flag
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
    pub fn new(wrap: Index) -> CountedIndex {
        validate_wrap(wrap);
        CountedIndex {
            val: AtomicUsize::new(0),
            mask: (wrap - 1) as usize,
        }
    }

    pub fn from_usize(val: usize, wrap: Index) -> CountedIndex {
        validate_wrap(wrap);
        CountedIndex {
            val: AtomicUsize::new(val),
            mask: (wrap - 1) as usize,
        }
    }

    pub fn wrap_at(&self) -> Index {
        self.mask as Index + 1
    }

    #[allow(dead_code)]
    // used by tests!
    #[inline(always)]
    pub fn load(&self, ord: Ordering) -> Index {
        (self.val.load(ord) & self.mask) as Index
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
    pub fn get_previous(start: usize, by: Index) -> usize {
        start.wrapping_sub(by as usize)
    }
}

impl<'a> Transaction<'a> {
    /// Loads the index, the expected valid flag, and the tag
    #[inline(always)]
    pub fn get(&self) -> (isize, usize) {
        ((self.loaded_vals & self.mask) as isize, self.loaded_vals)
    }

    /// Returns true if the values passed in matches the previous wrap-around of the Transaction
    #[inline(always)]
    pub fn matches_previous(&self, val: usize) -> bool {
        let wrap = self.mask.wrapping_add(1);
        rm_tag(self.loaded_vals.wrapping_sub(wrap)) == val
    }

    #[inline(always)]
    pub fn commit(self, by: Index, ord: Ordering) -> Option<Transaction<'a>> {
        let store_val = rm_tag(self.loaded_vals.wrapping_add(by as usize));
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
    pub fn commit_direct(self, by: Index, ord: Ordering) {
        let store_val = rm_tag(self.loaded_vals.wrapping_add(by as usize));
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

    fn test_incr_param(wrap_size: Index, goaround: usize) {
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

    fn test_incr_param_threaded(wrap_size: Index, goaround: usize, nthread: usize) {
        let mycounted = CountedIndex::new(wrap_size);
        scope(|scope| for _ in 0..nthread {
            scope.spawn(|| for _ in 0..goaround {
                for _ in 0..wrap_size {
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
        test_incr_param(1 + ::std::u16::MAX as Index, 2)
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
        test_incr_param_threaded(::std::u16::MAX as Index + 1, 2, 13)
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
