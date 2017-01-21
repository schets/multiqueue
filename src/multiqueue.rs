
use std::cell::Cell;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicUsize, fence};
use std::sync::atomic::Ordering::{Relaxed, Acquire, Release};

use alloc;
use countedindex::{CountedIndex, get_valid_wrap};
use maybe_acquire::{maybe_acquire_fence, MAYBE_ACQUIRE};

use read_cursor::{ReadCursor, Reader};

#[derive(Clone, Copy)]
enum QueueState {
    Single,
    Multi,
}

struct QueueEntry<T> {
    val: T,
    wraps: AtomicUsize,
}

/// A bounded queue that supports multiple reader and writers
/// and supports effecient methods for single consumers and producers
#[repr(C)]
struct MultiQueue<T> {
    d1: [u8; 64],

    // Writer data
    head: CountedIndex,
    tail_cache: AtomicUsize,
    writers: AtomicUsize,
    d2: [u8; 64],

    // Shared Data
    // The data and the wraps flag are in the same location
    // to reduce the # of distinct cache lines read when getting an item
    // The tail itself is rarely modified, making it a suitable candidate
    // to be in the shared space
    tail: ReadCursor,
    data: *mut QueueEntry<T>,
    capacity: isize,
    d3: [u8; 64],
}

pub struct MultiWriter<T> {
    queue: Arc<MultiQueue<T>>,
    state: Cell<QueueState>,
}

pub struct MultiReader<T> {
    queue: Arc<MultiQueue<T>>,
    reader: AtomicPtr<Reader>,
}

impl<T> MultiQueue<T> {
    pub fn new(_capacity: u32) -> (MultiWriter<T>, MultiReader<T>) {
        let capacity = get_valid_wrap(_capacity);
        let queuedat = alloc::allocate(capacity as usize);
        unsafe {
            for i in 0..capacity as isize {
                let elem: &QueueEntry<T> = &*queuedat.offset(i);
                elem.wraps.store(0, Relaxed);
            }
        }

        let (cursor, reader) = ReadCursor::new(capacity);

        let queue = MultiQueue {
            d1: unsafe { mem::uninitialized() },

            head: CountedIndex::new(capacity),
            tail_cache: AtomicUsize::new(0),
            writers: AtomicUsize::new(1),
            d2: unsafe { mem::uninitialized() },

            tail: cursor,
            data: queuedat,
            capacity: capacity as isize,

            d3: unsafe { mem::uninitialized() },
        };

        let qarc = Arc::new(queue);

        let mwriter = MultiWriter {
            queue: qarc.clone(),
            state: Cell::new(QueueState::Single),
        };

        let mreader = MultiReader {
            queue: qarc,
            reader: reader,
        };

        (mwriter, mreader)
    }

    pub fn push_multi(&self, val: T) -> Result<(), T> {
        let mut transaction = self.head.load_transaction(Relaxed);

        // This ensures that metadata about the cursor group is in cache
        self.tail.prefetch_metadata();
        unsafe {
            loop {
                let tail_cache = self.tail_cache.load(Acquire);
                if transaction.matches_previous(tail_cache) {
                    if transaction.matches_previous(self.reload_tail_multi(tail_cache)) {
                        return Err(val);
                    }
                }
                // This isize conversion is valid since indices are 30 bits
                let (chead, wrap_valid_tag) = transaction.get();
                let write_cell = &mut *self.data.offset(chead);
                match transaction.commit(1, Relaxed) {
                    Some(new_transaction) => transaction = new_transaction,
                    None => {
                        ptr::write(&mut write_cell.val, val);
                        write_cell.wraps.store(wrap_valid_tag, Release);
                        return Ok(());
                    }
                }
            }
        }
    }

    pub fn push_single(&self, val: T) -> Result<(), T> {
        let transaction = self.head.load_transaction(Relaxed);
        self.tail.prefetch_metadata();
        unsafe {
            if transaction.matches_previous(self.tail_cache.load(Relaxed)) {
                if transaction.matches_previous(self.reload_tail_single()) {
                    return Err(val);
                }
            }
            let (chead, wrap_valid_tag) = transaction.get();
            let write_cell = &mut *self.data.offset(chead);
            ptr::write(&mut write_cell.val, val);
            write_cell.wraps.store(wrap_valid_tag, Release);
            transaction.commit_direct(1, Relaxed);
            Ok(())
        }

        // Might consider letting the queue update the tail cache here preemptively
        // so it doesn't waste time before sending a message to do so
    }

    pub fn pop(&self, reader: &Reader) -> Option<T> {
        let mut ctail_attempt = reader.load_attempt(Relaxed);
        unsafe {
            loop {
                let (ctail, wrap_valid_tag) = ctail_attempt.get();
                let read_cell = &*self.data.offset(ctail);
                if read_cell.wraps.load(MAYBE_ACQUIRE) != wrap_valid_tag {
                    return None;
                }
                maybe_acquire_fence();
                let rval = ptr::read(&read_cell.val);
                match ctail_attempt.commit_attempt(1, Release) {
                    Some(new_attempt) => ctail_attempt = new_attempt,
                    None => return Some(rval),
                }
            }
        }
    }

    fn reload_tail_multi(&self, tail_cache: usize) -> usize {
        // This shows how far behind from head the reader is
        if let Some(max_diff_from_head) = self.tail.get_max_diff(self.head.load_count(Relaxed)) {
            let current_tail = self.head.get_previous(max_diff_from_head);
            match self.tail_cache.compare_exchange(tail_cache, current_tail, Relaxed, Acquire) {
                Ok(val) => val,
                Err(val) => val,
            }
        } else {
            self.tail_cache.load(Acquire)
        }
    }

    fn reload_tail_single(&self) -> usize {
        if let Some(max_diff_from_head) = self.tail.get_max_diff(self.head.load_count(Relaxed)) {
            let current_tail = self.head.get_previous(max_diff_from_head);
            self.tail_cache.store(current_tail, Relaxed);
            current_tail
        } else {
            // If this assert fires, memory has been corrupted
            assert!(false,
                    "The write head got ran over by consumers in isngle writer mode");
            0
        }
    }
}

impl<T> MultiWriter<T> {
    #[inline(always)]
    pub fn push(&self, val: T) -> Result<(), T> {
        match self.state.get() {
            QueueState::Single => self.queue.push_single(val),
            QueueState::Multi => {
                if self.queue.writers.load(Relaxed) == 1 {
                    fence(Acquire);
                    self.state.set(QueueState::Single);
                    self.queue.push_single(val)
                } else {
                    self.queue.push_multi(val)
                }
            }
        }
    }
}

impl<T> MultiReader<T> {
    pub fn pop(&self) -> Option<T> {
        unsafe { self.queue.pop(&*self.reader.load(Relaxed)) }
    }

    pub fn add_reader(&self) -> MultiReader<T> {
        MultiReader {
            queue: self.queue.clone(),
            reader: unsafe { self.queue.tail.add_reader(&*self.reader.load(Relaxed)) },
        }
    }
}

impl<T> Clone for MultiWriter<T> {
    fn clone(&self) -> MultiWriter<T> {
        self.state.set(QueueState::Multi);
        let rval = MultiWriter {
            queue: self.queue.clone(),
            state: Cell::new(QueueState::Multi),
        };
        self.queue.writers.fetch_add(1, Release);
        rval
    }
}

impl<T> Clone for MultiReader<T> {
    fn clone(&self) -> MultiReader<T> {
        let reader = self.reader.load(Relaxed);
        let rval = MultiReader {
            queue: self.queue.clone(),
            reader: AtomicPtr::new(reader),
        };
        unsafe {
            (*reader).dup_consumer();
        }
        rval
    }
}

impl<T> Drop for MultiWriter<T> {
    fn drop(&mut self) {
        self.queue.writers.fetch_sub(1, Release);
    }
}

impl<T> Drop for MultiReader<T> {
    fn drop(&mut self) {
        unsafe { (*self.reader.load(Relaxed)).remove_consumer() }
    }
}

unsafe impl<T> Sync for MultiQueue<T> {}
unsafe impl<T> Send for MultiQueue<T> {}
unsafe impl<T> Send for MultiWriter<T> {}
unsafe impl<T> Send for MultiReader<T> {}

pub fn multiqueue<T>(capacity: u32) -> (MultiWriter<T>, MultiReader<T>) {
    MultiQueue::new(capacity)
}

#[cfg(test)]
mod test {

    use super::*;
    use super::MultiQueue;

    extern crate crossbeam;
    use self::crossbeam::scope;

    use std::sync::atomic::Ordering::*;

    use std::sync::Barrier;

    #[test]
    fn build_queue() {
        let _ = MultiQueue::<usize>::new(10);
    }

    #[test]
    fn push_pop_test() {
        let (writer, reader) = MultiQueue::<usize>::new(1);
        for _ in 0..100 {
            assert!(reader.pop().is_none());
            writer.push(1 as usize).expect("Push should succeed");
            assert!(writer.push(1).is_err());
            assert_eq!(1, reader.pop().unwrap());
        }
    }

    fn spsc_broadcast(receivers: usize) {
        let (writer, reader) = MultiQueue::<usize>::new(10);
        let myb = Barrier::new(receivers + 1);
        let bref = &myb;
        let num_loop = 1000000;
        scope(|scope| {
            scope.spawn(move || {
                bref.wait();
                'outer: for i in 0..num_loop {
                    loop {
                        if writer.push(i).is_ok() {
                            break;
                        }
                    }
                }
            });
            for i in 0..(receivers - 1) {
                let this_reader = reader.add_reader();
                scope.spawn(move || {
                    bref.wait();
                    'outer: for i in 0..num_loop {
                        loop {
                            if let Some(val) = this_reader.pop() {
                                assert_eq!(i, val);
                                break;
                            }
                        }
                    }
                });
            }
            bref.wait();
            'outer: for i in 0..num_loop {
                loop {
                    if let Some(val) = reader.pop() {
                        assert_eq!(i, val);
                        break;
                    }
                }
            }
        });
        assert!(reader.pop().is_none());
    }

    #[test]
    fn test_spsc() {
        spsc_broadcast(1);
    }

    #[test]
    fn test_spsc_broadcast() {
        spsc_broadcast(3);
    }

}
