
use std::cell::Cell;
use std::fmt;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, fence};
use std::sync::atomic::Ordering::{Relaxed, Acquire, Release, AcqRel, SeqCst};
use std::sync::mpsc::{TrySendError, TryRecvError, RecvError};

use alloc;
use atomicsignal::LoadedSignal;
use countedindex::{CountedIndex, get_valid_wrap, is_tagged, rm_tag, Index, INITIAL_QUEUE_FLAG};
use maybe_acquire::{maybe_acquire_fence, MAYBE_ACQUIRE};
use memory::{MemoryManager, MemToken};
use wait::{Wait, BlockingWait};

use read_cursor::{ReadCursor, Reader};

#[derive(Clone, Copy)]
enum QueueState {
    Single,
    Multi,
}

/// This holds entries in the queue
struct QueueEntry<T> {
    val: T,
    wraps: AtomicUsize,
}

/// A bounded queue that supports multiple reader and writers
/// and supports effecient methods for single consumers and producers
#[repr(C)]
struct MultiQueue<T: Clone> {
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
    pub waiter: Box<Wait>,
    needs_notify: bool,
    d3: [u8; 64],

    pub manager: MemoryManager,
    d4: [u8; 64],
}

pub struct MultiWriter<T: Clone> {
    queue: Arc<MultiQueue<T>>,
    token: *const MemToken,
    state: Cell<QueueState>,
}

pub struct MultiReader<T: Clone> {
    queue: Arc<MultiQueue<T>>,
    reader: Reader,
    token: *const MemToken,
}

pub struct SingleReader<T: Clone> {
    reader: MultiReader<T>,
}

impl<T: Clone> MultiQueue<T> {
    pub fn new(_capacity: Index) -> (MultiWriter<T>, MultiReader<T>) {
        MultiQueue::new_with(_capacity, BlockingWait::new())
    }

    pub fn new_with<W: Wait + 'static>(_capacity: Index,
                                       wait: W)
                                       -> (MultiWriter<T>, MultiReader<T>) {
        let capacity = get_valid_wrap(_capacity);
        let queuedat = alloc::allocate(capacity as usize);
        unsafe {
            for i in 0..capacity as isize {
                let elem: &QueueEntry<T> = &*queuedat.offset(i);
                elem.wraps.store(INITIAL_QUEUE_FLAG, Relaxed);
            }
        }

        let (cursor, reader) = ReadCursor::new(capacity);
        let needs_notify = wait.needs_notify();
        let queue = MultiQueue {
            d1: unsafe { mem::uninitialized() },

            head: CountedIndex::new(capacity),
            tail_cache: AtomicUsize::new(0),
            writers: AtomicUsize::new(1),
            d2: unsafe { mem::uninitialized() },

            tail: cursor,
            data: queuedat,
            capacity: capacity as isize,
            waiter: Box::new(wait),
            needs_notify: needs_notify,
            d3: unsafe { mem::uninitialized() },

            manager: MemoryManager::new(),

            d4: unsafe { mem::uninitialized() },
        };

        let qarc = Arc::new(queue);

        let mwriter = MultiWriter {
            queue: qarc.clone(),
            state: Cell::new(QueueState::Single),
            token: qarc.manager.get_token(),
        };

        let mreader = MultiReader {
            queue: qarc.clone(),
            reader: reader,
            token: qarc.manager.get_token(),
        };

        (mwriter, mreader)
    }

    pub fn try_send_multi(&self, val: T) -> Result<(), TrySendError<T>> {
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
                        return Err(TrySendError::Full(val));
                    }
                } else {
                    maybe_acquire_fence();
                }
                match transaction.commit(1, Relaxed) {
                    Some(new_transaction) => transaction = new_transaction,
                    None => {
                        let current_tag = write_cell.wraps.load(Relaxed);
                        // Should probably make this check depend on a bit in
                        // the writer index, so the store afterward doesn't depend
                        // on both that load *and* this likely out-of-cache load
                        // right here. On Intel shouldn't affect throughput much but
                        // hits latency a bit, abd on arm there's a fence.
                        if is_tagged(current_tag) {
                            ptr::write(&mut write_cell.val, val);
                        } else {
                            write_cell.val = val;
                        }
                        write_cell.wraps.store(wrap_valid_tag, Release);
                        if self.needs_notify {
                            self.waiter.notify();
                        }
                        return Ok(());
                    }
                }
            }
        }
    }

    pub fn try_send_single(&self, val: T) -> Result<(), TrySendError<T>> {
        let transaction = self.head.load_transaction(Relaxed);
        let (chead, wrap_valid_tag) = transaction.get();
        self.tail.prefetch_metadata(); // See push_multi on this
        unsafe {
            let write_cell = &mut *self.data.offset(chead);
            let tail_cache = self.tail_cache.load(Relaxed);
            if transaction.matches_previous(tail_cache) {
                let new_tail = self.reload_tail_single(wrap_valid_tag);
                if transaction.matches_previous(new_tail) {
                    return Err(TrySendError::Full(val));
                }
            }
            transaction.commit_direct(1, Relaxed);
            let current_tag = write_cell.wraps.load(Relaxed);
            if is_tagged(current_tag) {
                ptr::write(&mut write_cell.val, val);
            } else {
                write_cell.val = val;
            }
            write_cell.wraps.store(wrap_valid_tag, Release);
            if self.needs_notify {
                self.waiter.notify();
            }
            Ok(())
        }
    }

    pub fn try_recv(&self, reader: &Reader) -> Result<T, (*const AtomicUsize, TryRecvError)> {
        let mut ctail_attempt = reader.load_attempt(Relaxed);
        unsafe {
            loop {
                let (ctail, wrap_valid_tag) = ctail_attempt.get();
                let read_cell = &*self.data.offset(ctail);

                // For any curious readers, this gnarly if block catchs a race between
                // advancing the write index and unsubscribing from the queue. in short,
                // Since unsubscribe happens after the read_cell is written, there's a race
                // between the first and second if statements. Hence, a second check is required
                // after the writer load so ensure that the the wrap_valid_tag is still wrong, and hence,
                // we had actually seen a race. Doing it this way gets rid of some fences on the fast path
                // so it's worth it. I didn't actually notice this until testing a wait condition which probably
                // got this to ping in such a way that the race happened.
                if rm_tag(read_cell.wraps.load(Acquire)) != wrap_valid_tag {
                    if self.writers.load(Relaxed) == 0 {
                        fence(Acquire);
                        if rm_tag(read_cell.wraps.load(Acquire)) != wrap_valid_tag {
                            return Err((ptr::null(), TryRecvError::Disconnected));
                        }
                    }
                    return Err((&read_cell.wraps, TryRecvError::Empty));
                }
                let rval = read_cell.val.clone();
                match ctail_attempt.commit_attempt(1, Release) {
                    Some(new_attempt) => {
                        ctail_attempt = new_attempt;
                    }
                    None => return Ok(rval),
                }
            }
        }
    }

    pub fn try_recv_view<R, F: FnOnce(&T) -> R>(&self,
                                                op: F,
                                                reader: &Reader)
                                                -> Result<R, (F, *const AtomicUsize, TryRecvError)> {
        let ctail_attempt = reader.load_attempt(Relaxed);
        unsafe {
            let (ctail, wrap_valid_tag) = ctail_attempt.get();
            let read_cell = &*self.data.offset(ctail);
            if rm_tag(read_cell.wraps.load(Acquire)) != wrap_valid_tag {
                if self.writers.load(Relaxed) == 0 {
                    fence(Acquire);
                    if rm_tag(read_cell.wraps.load(MAYBE_ACQUIRE)) != wrap_valid_tag {
                        return Err((op, ptr::null(), TryRecvError::Disconnected));
                    }
                }
                return Err((op, &read_cell.wraps, TryRecvError::Empty));
            }
            let rval = op(&read_cell.val);
            ctail_attempt.commit_direct(1, Release);
            Ok(rval)
        }
    }

    fn reload_tail_multi(&self, tail_cache: usize, count: usize) -> usize {
        if let Some(max_diff_from_head) = self.tail.get_max_diff(count) {
            let current_tail = CountedIndex::get_previous(count, max_diff_from_head);
            if tail_cache == current_tail {
                return current_tail;
            }
            match self.tail_cache.compare_exchange(tail_cache, current_tail, AcqRel, Relaxed) {
                Ok(_) => current_tail,
                Err(val) => val,
            }
        } else {
            self.tail_cache.load(Acquire)
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

impl<T: Clone> MultiWriter<T> {
    #[inline(always)]
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        let signal = self.queue.manager.signal.load(Relaxed);
        if signal.has_action() {
            let disconnected = self.handle_signals(signal);
            if disconnected {
                return Err(TrySendError::Full(val));
            }
        }
        match self.state.get() {
            QueueState::Single => self.queue.try_send_single(val),
            QueueState::Multi => {
                if self.queue.writers.load(Relaxed) == 1 {
                    fence(Acquire);
                    self.state.set(QueueState::Single);
                    self.queue.try_send_single(val)
                } else {
                    self.queue.try_send_multi(val)
                }
            }
        }
    }

    /// Removes the writer as a producer to the queue
    pub fn unsubscribe(self) {}

    #[cold]
    fn handle_signals(&self, signal: LoadedSignal) -> bool {
        if signal.get_epoch() {
            self.queue.manager.update_token(self.token);
        }
        signal.get_reader()
    }
}

impl<T: Clone> MultiReader<T> {
    /// Tries to receive a value from the queue without blocking.
    #[inline(always)]
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.examine_signals();
        match self.queue.try_recv(&self.reader) {
            Ok(v) => Ok(v),
            Err((_, e)) => Err(e),
        }
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        self.examine_signals();
        loop {
            match self.queue.try_recv(&self.reader) {
                Ok(v) => return Ok(v),
                Err((_, TryRecvError::Disconnected)) => return Err(RecvError),
                Err((pt, TryRecvError::Empty)) => {
                    let count = self.reader.load_count(Relaxed);
                    unsafe { self.queue.waiter.wait(count, &*pt); }
                }
            }
        }
    }


    pub fn add_reader(&self) -> MultiReader<T> {
        MultiReader {
            queue: self.queue.clone(),
            reader: self.queue.tail.add_reader(&self.reader, &self.queue.manager),
            token: self.queue.manager.get_token(),
        }
    }

    pub fn into_single(self) -> Result<SingleReader<T>, MultiReader<T>> {
        if self.reader.get_consumers() == 1 {
            fence(Acquire);
            Ok(SingleReader { reader: self })
        } else {
            Err(self)
        }
    }

    #[inline(always)]
    fn examine_signals(&self) {
        let signal = self.queue.manager.signal.load(Relaxed);
        if signal.has_action() {
            self.handle_signals(signal);
        }
    }

    #[cold]
    fn handle_signals(&self, signal: LoadedSignal) {
        if signal.get_epoch() {
            self.queue.manager.update_token(self.token);
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
    /// writer.try_send(1).expect("This will succeed since queue is empty");
    /// reader.try_recv().expect("This reader can read");
    /// assert!(writer.try_send(1).is_err(), "This fails since the reader2 group hasn't advanced");
    /// assert!(!reader_2_2.unsubscribe(), "This returns false since reader_2_1 is still alive");
    /// assert!(reader_2_1.unsubscribe(),
    ///         "This returns true since there are no readers alive in the reader_2_x group");
    /// writer.try_send(1).expect("This succeeds since the reader_2 group is not blocking");
    /// ```
    pub fn unsubscribe(self) -> bool {
        self.reader.get_consumers() == 1
    }
}

impl<T: Clone + Sync> SingleReader<T> {
    /// Identical to MultiReader::try_recv()
    #[inline(always)]
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.reader.try_recv()
    }

    #[inline(always)]
    pub fn recv(&self) -> Result<T, RecvError> {
        self.reader.recv()
    }

    /// Behaves almost
    #[inline(always)]
    pub fn try_recv_view<R, F: FnOnce(&T) -> R>(&self, op: F) -> Result<R, (F, TryRecvError)> {
        self.reader.examine_signals();
        match self.reader.queue.try_recv_view(op, &self.reader.reader) {
            Ok(v) => Ok(v),
            Err((op, _, e)) => Err((op, e)),
        }
    }

    pub fn recv_view<R, F: FnOnce(&T) -> R>(&self, mut op: F) -> Result<R, (F, RecvError)> {
        self.reader.examine_signals();
        loop {
            match self.reader.queue.try_recv_view(op, &self.reader.reader) {
                Ok(v) => return Ok(v),
                Err((o, _, TryRecvError::Disconnected)) => return Err((o, RecvError)),
                Err((o, pt, TryRecvError::Empty)) => {
                    op = o;
                    let count = self.reader.reader.load_count(Relaxed);
                    unsafe { self.reader.queue.waiter.wait(count, &*pt); }
                },
            }
        }
    }
    pub fn add_reader(&self) -> SingleReader<T> {
        self.reader.add_reader().into_single().unwrap()
    }


    /// Transforms the SingleReader into a MultiReader
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::multiqueue;
    /// let (_, mreader) = multiqueue::<usize>(6);
    /// let sreader = mreader.into_single().unwrap();
    /// let mreader2 = sreader.into_multi();
    /// // Can't use sreader anymore!
    /// // mreader.try_recv_view(|x| x+1) doesn't work since multireader can't do view methods
    ///
    ///
    /// ```
    pub fn into_multi(self) -> MultiReader<T> {
        self.reader
    }

    /// See MultiReader::unsubscribe()
    pub fn unsubscribe(self) -> bool {
        self.reader.unsubscribe()
    }
}

impl<T: Clone> Clone for MultiWriter<T> {
    /// Clones the writer, allowing multiple writers to push into the queue
    /// from different threads
    /// # Examples
    ///
    /// ```
    /// use multiqueue::multiqueue;
    /// let (writer, reader) = multiqueue(16);
    /// let writer2 = writer.clone();
    /// writer.try_send(1).unwrap();
    /// writer2.try_send(2).unwrap();
    /// assert_eq!(1, reader.try_recv().unwrap());
    /// assert_eq!(2, reader.try_recv().unwrap());
    /// ```
    fn clone(&self) -> MultiWriter<T> {
        self.state.set(QueueState::Multi);
        let rval = MultiWriter {
            queue: self.queue.clone(),
            state: Cell::new(QueueState::Multi),
            token: self.queue.manager.get_token(),
        };
        self.queue.writers.fetch_add(1, SeqCst);
        rval
    }
}

impl<T: Clone> Clone for MultiReader<T> {
    fn clone(&self) -> MultiReader<T> {
        self.reader.dup_consumer();
        MultiReader {
            queue: self.queue.clone(),
            reader: self.reader,
            token: self.queue.manager.get_token(),
        }
    }
}

impl<T: Clone> Drop for MultiWriter<T> {
    fn drop(&mut self) {
        self.queue.writers.fetch_sub(1, SeqCst);
        self.queue.manager.remove_token(self.token);
    }
}

impl<T: Clone> Drop for MultiReader<T> {
    fn drop(&mut self) {
        if self.reader.remove_consumer() == 1 {
            if self.queue.tail.remove_reader(&self.reader, &self.queue.manager) {
                self.queue.manager.signal.set_reader(SeqCst);
            }
            self.queue.manager.remove_token(self.token);
        }
    }
}

impl<T: Clone> Drop for MultiQueue<T> {
    fn drop(&mut self) {
        // everything that's tagged shouldn't be dropped
        // otherwise, everything else is valid and waiting to be read
        // or invalid and waiting to be overwritten/dropped
        for i in 0..self.capacity as isize {
            unsafe {
                let cell = &mut *self.data.offset(i);
                if !is_tagged(cell.wraps.load(Relaxed)) {
                    ptr::read(&cell.val);
                }
            }
        }
    }
}

impl<T: Clone> fmt::Debug for MultiReader<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Multireader generic error message!")
    }
}

unsafe impl<T: Clone> Sync for MultiQueue<T> {}
unsafe impl<T: Clone> Send for MultiQueue<T> {}
unsafe impl<T: Send + Clone> Send for MultiWriter<T> {}
unsafe impl<T: Send + Clone> Send for MultiReader<T> {}
unsafe impl<T: Send + Clone + Sync> Send for SingleReader<T> {}

pub fn multiqueue<T: Clone>(capacity: Index) -> (MultiWriter<T>, MultiReader<T>) {
    MultiQueue::new(capacity)
}

pub fn multiqueue_with<T: Clone, W: Wait + 'static>(capacity: Index,
                                                    wait: W)
                                                    -> (MultiWriter<T>, MultiReader<T>) {
    MultiQueue::new_with(capacity, wait)
}

#[cfg(test)]
mod test {

    use super::MultiQueue;

    extern crate crossbeam;
    use self::crossbeam::scope;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::sync::mpsc::TryRecvError;
    use std::thread::yield_now;

    #[test]
    fn build_queue() {
        let _ = MultiQueue::<usize>::new(10);
    }

    #[test]
    fn push_pop_test() {
        let (writer, reader) = MultiQueue::<usize>::new(1);
        for _ in 0..100 {
            assert!(reader.try_recv().is_err());
            writer.try_send(1 as usize).expect("Push should succeed");
            assert!(writer.try_send(1).is_err());
            assert_eq!(1, reader.try_recv().unwrap());
        }
    }

    fn mpsc_broadcast(senders: usize, receivers: usize) {
        let (writer, reader) = MultiQueue::<(usize, usize)>::new(4);
        let myb = Barrier::new(receivers + senders);
        let bref = &myb;
        let num_loop = 100000;
        scope(|scope| {
            for q in 0..senders {
                let cur_writer = writer.clone();
                scope.spawn(move || {
                    bref.wait();
                    'outer: for i in 0..num_loop {
                        for _ in 0..100000000 {
                            if cur_writer.try_send((q, i)).is_ok() {
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
                    bref.wait();
                    for _ in 0..num_loop * senders {
                        loop {
                            if let Ok(val) = this_reader.try_recv_view(|x| *x) {
                                assert_eq!(myv[val.0], val.1);
                                myv[val.0] += 1;
                                break;
                            }
                            yield_now();
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

    #[test]
    fn test_spsc_this() {
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
        assert!(writer.try_send(1).is_ok());
        let reader_2 = reader.add_reader();
        assert!(writer.try_send(1).is_err());
        assert_eq!(1, reader.try_recv().unwrap());
        assert!(reader.try_recv().is_err());
        assert_eq!(1, reader_2.try_recv().unwrap());
        assert!(reader_2.try_recv().is_err());
        assert!(writer.try_send(1).is_ok());
        assert!(writer.try_send(1).is_err());
        assert_eq!(1, reader.try_recv().unwrap());
        assert!(reader.try_recv().is_err());
        reader_2.unsubscribe();
        assert!(writer.try_send(2).is_ok());
        assert_eq!(2, reader.try_recv().unwrap());
    }

    fn mpmc_broadcast(senders: usize, receivers: usize, nclone: usize) {
        let (writer, reader) = MultiQueue::<usize>::new(10);
        let myb = Barrier::new((receivers * nclone) + senders);
        let bref = &myb;
        let num_loop = 1000000;
        let counter = AtomicUsize::new(0);
        let cref = &counter;
        scope(|scope| {
            for _ in 0..senders {
                let cur_writer = writer.clone();
                scope.spawn(move || {
                    bref.wait();
                    'outer: for _ in 0..num_loop {
                        for _ in 0..100000000 {
                            if cur_writer.try_send(1).is_ok() {
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
                let _this_reader = reader.add_reader();
                for _ in 0..nclone {
                    let this_reader = _this_reader.clone();
                    scope.spawn(move || {
                        bref.wait();
                        loop {
                            match this_reader.try_recv() {
                                Ok(_) => {
                                    cref.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(TryRecvError::Disconnected) => break,
                                _ => yield_now(),
                            }
                        }
                    });
                }
            }
            reader.unsubscribe();
        });
        assert_eq!(senders * receivers * num_loop,
                   counter.load(Ordering::SeqCst));
    }

    #[test]
    fn test_spmc() {
        mpmc_broadcast(1, 1, 2);
    }

    #[test]
    fn test_spmc_broadcast() {
        mpmc_broadcast(1, 2, 2);
    }

    #[test]
    fn test_mpmc() {
        mpmc_broadcast(2, 1, 2);
    }

    #[test]
    fn test_mpmc_broadcast() {
        mpmc_broadcast(2, 2, 2);
    }

    #[test]
    fn test_baddrop() {
        // This ensures that a bogus arc isn't dropped from the queue
        let (writer, reader) = MultiQueue::new(1);
        for _ in 0..10 {
            writer.try_send(Arc::new(10)).unwrap();
            reader.recv().unwrap();
        }
    }


    struct Dropper<'a> {
        aref: &'a AtomicUsize,
    }

    impl<'a> Dropper<'a> {
        pub fn new(a: &AtomicUsize) -> Dropper {
            a.fetch_add(1, Ordering::Relaxed);
            Dropper {
                aref: a,
            }
        }
    }

    impl<'a> Drop for Dropper<'a> {
        fn drop(& mut self) {
            self.aref.fetch_sub(1, Ordering::Relaxed);
        }
    }

    impl<'a> Clone for Dropper<'a> {
        fn clone(&self) -> Dropper<'a> {
            self.aref.fetch_add(1, Ordering::Relaxed);
            Dropper {
                aref: self.aref,
            }
        }
    }

    #[test]
    fn test_gooddrop() {
        // This counts the # of drops and creations
        let count = AtomicUsize::new(0);
        {
            let (writer, reader) = MultiQueue::new(1);
            for _ in 0..10 {
                writer.try_send(Dropper::new(&count)).unwrap();
                reader.recv().unwrap();
            }
        }
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }
}
