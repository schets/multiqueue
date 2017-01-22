
use std::cell::Cell;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicUsize, fence};
use std::sync::atomic::Ordering::{Relaxed, Acquire, Release, AcqRel};

use alloc;
use countedindex::{CountedIndex, get_valid_wrap, INITIAL_QUEUE_FLAG};
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
                elem.wraps.store(INITIAL_QUEUE_FLAG, Relaxed);
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

        // This tries to ensure the tail fetch metadata is always in the cache
        // The effect of this is that whenever one has to find the minimum tail,
        // the data about the loop is in-cache so that whole loop executes deep in
        // an out-of-order engine while the branch predictor predicts there is more space
        // and continues on pushing
        self.tail.prefetch_metadata();
        unsafe {
            loop {
                let (chead, wrap_valid_tag) = transaction.get();
                let write_cell = &mut *self.data.offset(chead);
                let tail_cache = self.tail_cache.load(Relaxed);
                if transaction.matches_previous(tail_cache) {
                    let new_tail = self.reload_tail_multi(tail_cache, wrap_valid_tag);
                    if transaction.matches_previous(new_tail) {
                        return Err(val);
                    }
                } else {
                    // In the other case, there's already an acquire load for tail_cache.
                    // This speeds up the full queue case for arm
                    maybe_acquire_fence();
                }
                // This isize conversion is valid since indices are 30 bits
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
        let (chead, wrap_valid_tag) = transaction.get();
        self.tail.prefetch_metadata(); // See push_multi on this
        unsafe {
            let write_cell = &mut *self.data.offset(chead);
            let tail_cache = self.tail_cache.load(Relaxed);
            if transaction.matches_previous(tail_cache) {
                let new_tail = self.reload_tail_single(wrap_valid_tag);
                if transaction.matches_previous(new_tail) {
                    return Err(val);
                }
            }
            ptr::write(&mut write_cell.val, val);
            write_cell.wraps.store(wrap_valid_tag, Release);
            transaction.commit_direct(1, Relaxed);
            Ok(())
        }
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
                    Some(new_attempt) => {
                        ctail_attempt = new_attempt;
                        mem::forget(rval);
                    }
                    None => return Some(rval),
                }
            }
        }
    }

    fn reload_tail_multi(&self, tail_cache: usize, count: usize) -> usize {
        if let Some(max_diff_from_head) = self.tail.get_max_diff(count) {
            let current_tail = CountedIndex::get_previous(count, max_diff_from_head);
            if tail_cache == current_tail {
                return current_tail;
            }
            match self.tail_cache.compare_exchange(tail_cache, current_tail, AcqRel, Relaxed) {
                Ok(val) => current_tail,
                Err(val) => val,
            }
        } else {
            let rval = self.tail_cache.load(Acquire);
            rval
        }
    }

    fn reload_tail_single(&self, count: usize) -> usize {
        let max_diff_from_head = self.tail
            .get_max_diff(count)
            .expect("The write head got ran over by consumers in single writer mode. This \
                     process is borked!");
        let current_tail = CountedIndex::get_previous(count, max_diff_from_head);
        self.tail_cache.store(current_tail, Relaxed);
        current_tail
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
    #[inline(always)]
    pub fn pop(&self) -> Option<T> {
        unsafe { self.queue.pop(&*self.reader.load(Relaxed)) }
    }

    pub fn add_reader(&self) -> MultiReader<T> {
        MultiReader {
            queue: self.queue.clone(),
            reader: unsafe { self.queue.tail.add_reader(&*self.reader.load(Relaxed)) },
        }
    }

    /// Removes the given reader from the queue subscription lib
    /// Returns true if this is the last reader in a given broadcast unit
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::multiqueue;
    /// let (writer, reader) = multiqueue(1);
    /// let reader_2_1 = reader.add_reader();
    /// let reader_2_2 = reader_2_1.clone();
    /// writer.push(1).expect("This will succeed since queue is empty");
    /// reader.pop().expect("This reader can read");
    /// assert!(writer.push(1).is_err(), "This fails since the reader2 group hasn't advanced");
    /// assert!(!reader_2_2.unsubscribe(), "This returns false since reader_2_1 is still alive");
    /// assert!(reader_2_1.unsubscribe(),
    ///         "This returns true since there are no readers alive in the reader_2_x group");
    /// writer.push(1).expect("This succeeds since ");
    /// ```
    pub fn unsubscribe(self) -> bool {
        unsafe {
            let reader = &*self.reader.load(Relaxed);
            reader.get_consumers() == 1
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
        unsafe {
            let reader = &*self.reader.load(Relaxed);
            if reader.remove_consumer() == 1 {
                self.queue.tail.remove_reader(reader);
            }
        }
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
    use std::thread::yield_now;

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

    fn mpsc_broadcast(senders: usize, receivers: usize) {
        let (writer, reader) = MultiQueue::<(usize, usize)>::new(10);
        let myb = Barrier::new(receivers + senders);
        let bref = &myb;
        let num_loop = 100000;
        scope(|scope| {
            for q in 0..senders {
                let cur_writer = writer.clone();
                scope.spawn(move || {
                    bref.wait();
                    'outer: for i in 0..num_loop {
                        for j in 0..100000000 {
                            if cur_writer.push((q, i)).is_ok() {
                                continue 'outer;
                            }
                            yield_now();
                        }
                        assert!(false, "Writer could not write");
                    }
                });
            }
            {
                let _ = writer;
            }; // Dump writer so we don't waste too much time on it
            for _ in 0..receivers {
                let this_reader = reader.add_reader();
                scope.spawn(move || {
                    let mut myv = Vec::new();
                    for _ in 0..senders {
                        myv.push(0);
                    }
                    bref.wait();
                    for _ in 0..num_loop * senders {
                        loop {
                            if let Some(val) = this_reader.pop() {
                                assert_eq!(myv[val.0], val.1);
                                myv[val.0] += 1;
                                break;
                            }
                            yield_now();
                        }
                    }
                    assert!(this_reader.pop().is_none());
                });
            }
            {
                let todie = reader;
            }
        });
    }

    #[test]
    fn test_spsc() {
        mpsc_broadcast(1, 1);
    }

    #[test]
    fn test_spsc_broadcast() {
        mpsc_broadcast(1, 3);
    }

    #[test]
    fn test_mpsc_single() {
        mpsc_broadcast(2, 1);
    }

    #[test]
    fn test_mpsc_broadcast() {
        mpsc_broadcast(2, 3);
    }

    #[test]
    fn test_remove_reader() {
        let (writer, reader) = MultiQueue::<usize>::new(1);
        assert!(writer.push(1).is_ok());
        let reader_2 = reader.add_reader();
        assert!(writer.push(1).is_err());
        assert_eq!(1, reader.pop().unwrap());
        assert!(reader.pop().is_none());
        assert_eq!(1, reader_2.pop().unwrap());
        assert!(reader_2.pop().is_none());
        assert!(writer.push(1).is_ok());
        assert!(writer.push(1).is_err());
        assert_eq!(1, reader.pop().unwrap());
        assert!(reader.pop().is_none());
        {
            let to_die = reader_2;
        }
        assert!(writer.push(2).is_ok());
        assert_eq!(2, reader.pop().unwrap());
    }

    fn mpmc_broadcast(senders: usize, receivers: usize) {
        let (writer, reader) = MultiQueue::<(usize, usize)>::new(10);
        let myb = Barrier::new(receivers + senders);
        let bref = &myb;
        let num_loop = 100000;
        scope(|scope| {
            for q in 0..senders {
                let cur_writer = writer.clone();
                scope.spawn(move || {
                    bref.wait();
                    'outer: for i in 0..num_loop {
                        for j in 0..100000000 {
                            if cur_writer.push((q, i)).is_ok() {
                                continue 'outer;
                            }
                            yield_now();
                        }
                        assert!(false, "Writer could not write");
                    }
                });
            }
            {
                let todie = writer;
            }; // Dump writer so we don't waste too much time on it
            for _ in 0..receivers {
                let this_reader = reader.add_reader();
                scope.spawn(move || {
                    let mut myv = Vec::new();
                    for _ in 0..senders {
                        myv.push(0);
                    }
                    bref.wait();
                    for _ in 0..num_loop * senders {
                        loop {
                            if let Some(val) = this_reader.pop() {
                                assert_eq!(myv[val.0], val.1);
                                myv[val.0] += 1;
                                break;
                            }
                            yield_now();
                        }
                    }
                    assert!(this_reader.pop().is_none());
                });
            }
            {
                let todie = reader;
            }
        });
    }



}
