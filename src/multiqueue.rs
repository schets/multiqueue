
use std::any::Any;
use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, fence};
use std::sync::atomic::Ordering::*;
use std::sync::mpsc::{TrySendError, TryRecvError, RecvError};
use std::thread::yield_now;

use alloc;
use atomicsignal::LoadedSignal;
use countedindex::{CountedIndex, get_valid_wrap, is_tagged, rm_tag, Index, INITIAL_QUEUE_FLAG};
use maybe_acquire::{maybe_acquire_fence, MAYBE_ACQUIRE};
use memory::{MemoryManager, MemToken};
use wait::*;

use read_cursor::{ReadCursor, Reader};

extern crate futures;
extern crate parking_lot;
extern crate smallvec;

use self::futures::{Async, AsyncSink, Poll, Sink, Stream, StartSend};
use self::futures::task::{park, Task};

/// This is basically acting as a static bool
/// so the queue can act as a normal mpmc in other circumstances
trait DoDrop<T> {
    fn do_drop() -> bool;
    fn get_val(&mut T) -> T;
    fn forget_val(T);
}

struct YesDrop<T> {
    mk: PhantomData<T>,
}

impl<T: Clone> DoDrop<T> for YesDrop<T> {
    #[inline(always)]
    fn do_drop() -> bool {
        true
    }

    #[inline(always)]
    fn get_val(val: &mut T) -> T{
        val.clone()
    }

    #[inline(always)]
    fn forget_val(_v: T) {}
}


struct NoDrop<T> {
    mk: PhantomData<T>,
}

impl<T> DoDrop<T> for NoDrop<T> {
    #[inline(always)]
    fn do_drop() -> bool {
        false
    }

    #[inline(always)]
    fn get_val(val: &mut T) -> T {
        unsafe { ptr::read(val) }
    }

    #[inline(always)]
    fn forget_val(val: T) {
        mem::forget(val);
    }
}

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
    pub waiter: Arc<Wait>,
    needs_notify: bool,
    d3: [u8; 64],

    pub manager: MemoryManager,
    d4: [u8; 64],
}

/// This class is the sending half of the MultiQueue. It supports both
/// single and multi consumer modes with competitive performance in each case.
/// It only supports nonblocking writes (the futures sender being an exception)
/// as well as being the conduit for adding new writers.
///
/// # Examples
///
/// ```
/// use std::thread;
///
/// let (send, recv) = multiqueue::multiqueue(4);
///
/// let mut handles = vec![];
///
/// for i in 0..2 { // or n
///     let cur_recv = recv.add_stream();
///     for j in 0..2 {
///         let stream_consumer = cur_recv.clone();
///         handles.push(thread::spawn(move || {
///             for val in stream_consumer {
///                 println!("Stream {} consumer {} got {}", i, j, val);
///             }
///         }));
///     }
///     // cur_recv is dropped here
/// }
///
/// // Take notice that I drop the reader - this removes it from
/// // the queue, meaning that the readers in the new threads
/// // won't get starved by the lack of progress from recv
/// recv.unsubscribe();
///
/// for i in 0..10 {
///     // Don't do this busy loop in real stuff unless you're really sure
///     loop {
///         if send.try_send(i).is_ok() {
///             break;
///         }
///     }
/// }
/// drop(send);
///
/// for t in handles {
///     t.join();
/// }
/// // prints along the lines of
/// // Stream 0 consumer 1 got 2
/// // Stream 0 consumer 0 got 0
/// // Stream 1 consumer 0 got 0
/// // Stream 0 consumer 1 got 1
/// // Stream 1 consumer 1 got 1
/// // Stream 1 consumer 0 got 2
/// // etc
/// ```
pub struct Sender<T: Clone> {
    queue: Arc<MultiQueue<T>>,
    token: *const MemToken,
    state: Cell<QueueState>,
}

/// This class is the receiving half of the MultiQueue.
/// Within each stream, it supports both single and multi consumer modes
/// with competitive performance in each case. It supports blocking and
/// nonblocking read modes as well as being the conduit for adding
/// new streams.
///
/// # Examples
///
/// ```
/// use std::thread;
///
/// let (send, recv) = multiqueue::multiqueue(4);
///
/// let mut handles = vec![];
///
/// for i in 0..2 { // or n
///     let cur_recv = recv.add_stream();
///     for j in 0..2 {
///         let stream_consumer = cur_recv.clone();
///         handles.push(thread::spawn(move || {
///             for val in stream_consumer {
///                 println!("Stream {} consumer {} got {}", i, j, val);
///             }
///         }));
///     }
///     // cur_recv is dropped here
/// }
///
/// // Take notice that I drop the reader - this removes it from
/// // the queue, meaning that the readers in the new threads
/// // won't get starved by the lack of progress from recv
/// recv.unsubscribe();
///
/// for i in 0..10 {
///     // Don't do this busy loop in real stuff unless you're really sure
///     loop {
///         if send.try_send(i).is_ok() {
///             break;
///         }
///     }
/// }
/// drop(send);
///
/// for t in handles {
///     t.join();
/// }
/// // prints along the lines of
/// // Stream 0 consumer 1 got 2
/// // Stream 0 consumer 0 got 0
/// // Stream 1 consumer 0 got 0
/// // Stream 0 consumer 1 got 1
/// // Stream 1 consumer 1 got 1
/// // Stream 1 consumer 0 got 2
/// // etc
/// ```
pub struct Receiver<T: Clone> {
    queue: Arc<MultiQueue<T>>,
    reader: Reader,
    token: *const MemToken,
    alive: bool,
}

/// This class is similar to the receiver, except it ensures that there
/// is only one consumer for the stream it owns. This means that
/// one can safely view the data in-place with the recv_view method family
/// and avoid the cost of copying it. If there's only one receiver on a stream,
/// it can be converted into a SingleReceiver
///
/// # Example:
///
/// ```
/// use multiqueue::multiqueue;
///
/// let (w, r) = multiqueue(10);
/// w.try_send(1).unwrap();
/// let r2 = r.clone();
/// // Fails since there's two receivers on the stream
/// assert!(r2.into_single().is_err());
/// let single_r = r.into_single().unwrap();
/// let val = match single_r.try_recv_view(|x| 2 * *x) {
///     Ok(val) => val,
///     Err(_) => panic!("Queue should have an element"),
/// };
/// assert_eq!(2, val);
/// ```
pub struct SingleReceiver<T: Clone> {
    reader: Receiver<T>,
}

/// This is a sender that can transparently act as a futures stream
#[derive(Clone)]
pub struct FuturesSender<T: Clone> {
    writer: Sender<T>,
    wait: Arc<FuturesWait>,
    prod_wait: Arc<FuturesWait>,
}

/// This is a receiver that can transparently act as a futures stream
#[derive(Clone)]
pub struct FuturesReceiver<T: Clone> {
    reader: Receiver<T>,
    wait: Arc<FuturesWait>,
    prod_wait: Arc<FuturesWait>,
}

/// This is a single receiver that can transparently act as a futures stream
pub struct FuturesSingleReceiver<R, F: FnMut(&T) -> R, T: Clone> {
    reader: SingleReceiver<T>,
    wait: Arc<FuturesWait>,
    prod_wait: Arc<FuturesWait>,
    op: F,
}

struct FuturesWait {
    spins_first: usize,
    spins_yield: usize,
    parked: parking_lot::Mutex<Vec<Task>>,
}

impl<T: Clone> MultiQueue<T> {
    pub fn new(_capacity: Index) -> (Sender<T>, Receiver<T>) {
        MultiQueue::new_with(_capacity, BlockingWait::new())
    }

    pub fn new_with<W: Wait + 'static>(capacity: Index, wait: W) -> (Sender<T>, Receiver<T>) {
        MultiQueue::new_internal(capacity, Arc::new(wait))
    }

    fn new_internal(_capacity: Index, wait: Arc<Wait>) -> (Sender<T>, Receiver<T>) {
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
            waiter: wait,
            needs_notify: needs_notify,
            d3: unsafe { mem::uninitialized() },

            manager: MemoryManager::new(),

            d4: unsafe { mem::uninitialized() },
        };

        let qarc = Arc::new(queue);

        let mwriter = Sender {
            queue: qarc.clone(),
            state: Cell::new(QueueState::Single),
            token: qarc.manager.get_token(),
        };

        let mreader = Receiver {
            queue: qarc.clone(),
            reader: reader,
            token: qarc.manager.get_token(),
            alive: true,
        };

        (mwriter, mreader)
    }

    pub fn try_send_multi<D: DoDrop<T>>(&self, val: T) -> Result<(), TrySendError<T>> {
        let mut transaction = self.head.load_transaction(Relaxed);

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

                        // This will delay the dropping of the exsisting item until
                        // after the write is done. This will have a marginal effect on
                        // throughput in most cases but will really help latency.
                        // Hopefully the compiler is smart enough to get rid of this
                        // when there's no drop
                        let _possible_drop = if D::do_drop() && !is_tagged(current_tag) {
                            Some(ptr::read(&write_cell.val))
                        } else {
                            None
                        };
                        ptr::write(&mut write_cell.val, val);
                        write_cell.wraps.store(wrap_valid_tag, Release);

                        // This tries to ensure the tail fetch metadata is always in the cache
                        // The effect of this is that whenever one has to find the minimum tail,
                        // the data about the loop is in-cache so that whole loop executes deep in
                        // an out-of-order engine while the branch predictor
                        // predicts there is more space and continues on pushing
                        self.tail.prefetch_metadata();
                        return Ok(());
                    }
                }
            }
        }
    }

    pub fn try_send_single(&self, val: T) -> Result<(), TrySendError<T>> {
        let transaction = self.head.load_transaction(Relaxed);
        let (chead, wrap_valid_tag) = transaction.get();
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
            let _possible_drop = if !is_tagged(current_tag) {
                Some(ptr::read(&write_cell.val))
            } else {
                None
            };
            ptr::write(&mut write_cell.val, val);
            write_cell.wraps.store(wrap_valid_tag, Release);
            self.tail.prefetch_metadata(); // See push_multi on this
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
                // after the writer load so ensure that the the wrap_valid_tag is still wrong so
                // we had actually seen a race. Doing it this way removes fences on the fast path
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

    pub fn try_recv_view<R, F: FnOnce(&T) -> R>
        (&self,
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

impl<T: Clone> Sender<T> {
    #[inline(always)]
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        let signal = self.queue.manager.signal.load(Relaxed);
        if signal.has_action() {
            let disconnected = self.handle_signals(signal);
            if disconnected {
                return Err(TrySendError::Full(val));
            }
        }
        let val = match self.state.get() {
            QueueState::Single => self.queue.try_send_single(val),
            QueueState::Multi => {
                if self.queue.writers.load(Relaxed) == 1 {
                    fence(Acquire);
                    self.state.set(QueueState::Single);
                    self.queue.try_send_single(val)
                } else {
                    self.queue.try_send_multi::<YesDrop<T>>(val)
                }
            }
        };
        // Putting this in the send functions
        // greatly confuses the compiler and literally halfs
        // the performance of the queue
        if val.is_ok() {
            if self.queue.needs_notify {
                self.queue.waiter.notify();
            }
        }
        val
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

impl<T: Clone> Receiver<T> {
    /// Tries to receive a value from the queue without blocking.
    ///
    /// # Examples:
    ///
    /// ```
    /// use multiqueue::multiqueue;
    /// let (w, r) = multiqueue(10);
    /// w.try_send(1).unwrap();
    /// assert_eq!(1, r.try_recv().unwrap());
    /// ```
    ///
    /// ```
    /// use multiqueue::multiqueue;
    /// use std::thread;
    ///
    /// let (send, recv) = multiqueue(10);
    ///
    /// let handle = thread::spawn(move || {
    ///     for val in recv {
    ///         println!("Got {}", val);
    ///     }
    /// });
    ///
    /// for i in 0..10 {
    ///     send.try_send(i).unwrap();
    /// }
    ///
    /// // Drop the sender to close the queue
    /// drop(send);
    ///
    /// handle.join();
    /// ```
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
                    unsafe {
                        self.queue.waiter.wait(count, &*pt, &self.queue.writers);
                    }
                }
            }
        }
    }

    /// Adds a new data stream to the queue, starting at the same position
    /// as the Receiver this is being called on.
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::multiqueue;
    /// let (w, r) = multiqueue(10);
    /// w.try_send(1).unwrap();
    /// assert_eq!(r.recv().unwrap(), 1);
    /// w.try_send(1).unwrap();
    /// let r2 = r.add_stream();
    /// assert_eq!(r.recv().unwrap(), 1);
    /// assert_eq!(r2.recv().unwrap(), 1);
    /// assert!(r.try_recv().is_err());
    /// assert!(r2.try_recv().is_err());
    /// ```
    ///
    /// ```
    /// use multiqueue::multiqueue;
    ///
    /// use std::thread;
    ///
    /// let (send, recv) = multiqueue(4);
    /// let mut handles = vec![];
    /// for i in 0..2 { // or n
    ///     let cur_recv = recv.add_stream();
    ///     handles.push(thread::spawn(move || {
    ///         for val in cur_recv {
    ///             println!("Stream {} got {}", i, val);
    ///         }
    ///     }));
    /// }
    ///
    /// // Take notice that I drop the reader - this removes it from
    /// // the queue, meaning that the readers in the new threads
    /// // won't get starved by the lack of progress from recv
    /// recv.unsubscribe();
    ///
    /// for i in 0..10 {
    ///     // Don't do this busy loop in real stuff unless you're really sure
    ///     loop {
    ///         if send.try_send(i).is_ok() {
    ///             break;
    ///         }
    ///     }
    /// }
    ///
    /// // Drop the sender to close the queue
    /// drop(send);
    ///
    /// for t in handles {
    ///     t.join();
    /// }
    ///
    /// ```
    pub fn add_stream(&self) -> Receiver<T> {
        Receiver {
            queue: self.queue.clone(),
            reader: self.queue.tail.add_stream(&self.reader, &self.queue.manager),
            token: self.queue.manager.get_token(),
            alive: true,
        }
    }

    /// If there is only one Receiver on the stream, converts the
    /// Receiver into a SingleReceiver otherwise returns the Receiver.
    ///
    /// # Example:
    ///
    /// ```
    /// use multiqueue::multiqueue;
    ///
    /// let (w, r) = multiqueue(10);
    /// w.try_send(1).unwrap();
    /// let r2 = r.clone();
    /// // Fails since there's two receivers on the stream
    /// assert!(r2.into_single().is_err());
    /// let single_r = r.into_single().unwrap();
    /// let val = match single_r.try_recv_view(|x| 2 * *x) {
    ///     Ok(val) => val,
    ///     Err(_) => panic!("Queue should have an element"),
    /// };
    /// assert_eq!(2, val);
    /// ```
    pub fn into_single(self) -> Result<SingleReceiver<T>, Receiver<T>> {
        if self.reader.get_consumers() == 1 {
            fence(Acquire);
            self.reader.set_single();
            Ok(SingleReceiver { reader: self })
        } else {
            Err(self)
        }
    }

    /// Returns a non-owning iterator that iterates over the queue
    /// until it fails to receive an item, either through being empty
    /// or begin disconnected. This iterator will never block.
    ///
    /// # Examples:
    ///
    /// ```
    /// use multiqueue::multiqueue;
    /// let (w, r) = multiqueue(2);
    /// for _ in 0 .. 3 {
    ///     w.try_send(1).unwrap();
    ///     w.try_send(2).unwrap();
    ///     for val in r.partial_iter().zip(1..2) {
    ///         assert_eq!(val.0, val.1);
    ///     }
    /// }
    /// ```
    pub fn partial_iter<'a>(&'a self) -> RecvPartialIterator<'a, T> {
        RecvPartialIterator { reader: self }
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
    /// let reader_2_1 = reader.add_stream();
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

    /// Runs the passed function after unsubscribing the reader from the queue
    fn do_unsubscribe_with<F: FnOnce()>(&mut self, f: F) {
        if self.alive {
            self.alive = false;
            if self.reader.remove_consumer() == 1 {
                if self.queue.tail.remove_reader(&self.reader, &self.queue.manager) {
                    self.queue.manager.signal.set_reader(SeqCst);
                }
                self.queue.manager.remove_token(self.token);
            }
            fence(SeqCst);
            f()
        }
    }
}

impl<T: Clone + Sync> SingleReceiver<T> {
    /// Identical to Receiver::try_recv()
    #[inline(always)]
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.reader.try_recv()
    }

    // Identical to Receiver::recv
    #[inline(always)]
    pub fn recv(&self) -> Result<T, RecvError> {
        self.reader.recv()
    }

    /// Applies the passed function to the value in the queue without copying it out
    /// If there is no data in the queue or the writers have disconnected,
    /// returns an Err((F, TryRecvError))
    ///
    /// # Example
    /// ```
    /// use multiqueue::multiqueue;
    ///
    /// let (w, r) = multiqueue(10);
    /// let single_r = r.into_single().unwrap();
    /// for i in 0..5 {
    ///     w.try_send(i).unwrap();
    /// }
    ///
    /// for i in 0..5 {
    ///     let val = match single_r.recv_view(|x| 1 + *x) {
    ///         Ok(val) => val,
    ///         Err(_) => panic!("Queue shouldn't be disconncted or empty"),
    ///     };
    ///     assert_eq!(i + 1, val);
    /// }
    /// drop(w);
    /// assert!(single_r.recv_view(|x| *x).is_err());
    /// ```
    #[inline(always)]
    pub fn try_recv_view<R, F: FnOnce(&T) -> R>(&self, op: F) -> Result<R, (F, TryRecvError)> {
        self.reader.examine_signals();
        match self.reader.queue.try_recv_view(op, &self.reader.reader) {
            Ok(v) => Ok(v),
            Err((op, _, e)) => Err((op, e)),
        }
    }

    /// Applies the passed function to the value in the queue without copying it out
    /// If there is no data in the queue, blocks until an item is pushed into the queue
    /// or all writers disconnect
    ///
    /// # Example
    /// ```
    /// use multiqueue::multiqueue;
    ///
    /// let (w, r) = multiqueue(10);
    /// let single_r = r.into_single().unwrap();
    /// for i in 0..5 {
    ///     w.try_send(i).unwrap();
    /// }
    ///
    /// for i in 0..5 {
    ///     let val = match single_r.recv_view(|x| 1 + *x) {
    ///         Ok(val) => val,
    ///         Err(_) => panic!("Queue shouldn't be disconncted or empty"),
    ///     };
    ///     assert_eq!(i + 1, val);
    /// }
    /// drop(w);
    /// assert!(single_r.recv_view(|x| *x).is_err());
    /// ```
    pub fn recv_view<R, F: FnOnce(&T) -> R>(&self, mut op: F) -> Result<R, (F, RecvError)> {
        self.reader.examine_signals();
        loop {
            match self.reader.queue.try_recv_view(op, &self.reader.reader) {
                Ok(v) => return Ok(v),
                Err((o, _, TryRecvError::Disconnected)) => return Err((o, RecvError)),
                Err((o, pt, TryRecvError::Empty)) => {
                    op = o;
                    let count = self.reader.reader.load_count(Relaxed);
                    unsafe {
                        self.reader.queue.waiter.wait(count, &*pt, &self.reader.queue.writers);
                    }
                }
            }
        }
    }

    /// This adds a new stream with a Single Receiver, otherwise it
    /// is identical to Receiver::add_stream()
    pub fn add_stream(&self) -> SingleReceiver<T> {
        self.reader.add_stream().into_single().unwrap()
    }

    /// Returns an iterator acting over the results of the SingleReceiver
    /// with the given function called on them. Essentially the loop
    /// match self.recv() {...} in iterator form
    ///
    /// Notes: The original reader an be obtained fom the iterator with the function get_reader
    ///
    /// #Examples
    /// ```
    /// use multiqueue::multiqueue;
    ///
    /// let (w, r) = multiqueue(10);
    /// for i in 0..5 {
    ///     w.try_send(i).unwrap();
    /// }
    /// drop(w);
    /// for val in r.into_single().unwrap().iter_with(|x| *x + 1).zip(1..6) {
    ///     assert_eq!(val.0, val.1);
    /// }
    /// ```
    pub fn iter_with<R, F: FnMut(&T) -> R>(self, op: F) -> RecvViewIterator<R, F, T> {
        RecvViewIterator { vals: (op, self) }
    }

    /// Returns an iterator acting over the results of the SingleReceiver
    /// with the given function called on them in a similar fashion to iter_with,
    /// except this iterator exits once the queue is empty and does not take ownership of
    /// the receiver
    ///
    /// Notes: The original reader an be obtained fom the iterator with the function get_reader
    ///
    /// #Examples
    /// ```
    /// use multiqueue::multiqueue;
    ///
    /// let (w, r) = multiqueue(10);
    /// let single_r = r.into_single().unwrap();
    /// for _ in 0..2 {
    ///     for i in 0..5 {
    ///         w.try_send(i).unwrap();
    ///     }
    ///     for val in single_r.partial_iter_with(|x| *x + 1).zip(1..6) {
    ///         assert_eq!(val.0, val.1);
    ///     }
    /// }
    /// ```

    pub fn partial_iter_with<'a, R, F: FnMut(&T) -> R>(&'a self,
                                                       op: F)
                                                       -> RecvPartialViewIterator<'a, R, F, T> {
        RecvPartialViewIterator { vals: (op, self) }
    }


    /// Transforms the SingleReceiver into a Receiver
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::multiqueue;
    /// let (_, mreader) = multiqueue::<usize>(6);
    /// let sreader = mreader.into_single().unwrap();
    /// let _mreader2 = sreader.into_multi();
    /// // Can't use sreader anymore!
    /// // mreader.try_recv_view(|x| x+1) doesn't work since multireader can't do view methods
    /// ```
    pub fn into_multi(self) -> Receiver<T> {
        self.reader
    }

    /// See Receiver::unsubscribe()
    pub fn unsubscribe(self) -> bool {
        self.reader.unsubscribe()
    }
}

impl<T: Clone> FuturesSender<T> {
    /// Identical to Sender::try_send()
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        self.writer.try_send(val)
    }

    /// Identical to Sender::unsubscribe()
    pub fn unsubscribe(self) {
        self.writer.unsubscribe()
    }
}

impl<T: Clone> FuturesReceiver<T> {
    /// Identical to Receiver::try_recv()
    #[inline(always)]
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.reader.try_recv()
    }

    /// Creates a new stream and returns a FuturesReceiver on that stream
    pub fn add_stream(&self) -> FuturesReceiver<T> {
        let rx = self.reader.add_stream();
        FuturesReceiver {
            reader: rx,
            wait: self.wait.clone(),
            prod_wait: self.prod_wait.clone(),
        }
    }

    /// Attempts to transform this receiver into a FuturesSingleReceiver
    /// calling the passed function on the input data.
    pub fn into_single<R, F: FnMut(&T) -> R>
        (self,
         op: F)
         -> Result<FuturesSingleReceiver<R, F, T>, (F, FuturesReceiver<T>)> {
        let new_mreader;
        let new_pwait = self.prod_wait.clone();
        let new_wait = self.wait.clone();
        {
            new_mreader = self.reader.clone();
            drop(self);
        }
        match new_mreader.into_single() {
            Ok(sreader) => {
                Ok(FuturesSingleReceiver {
                    reader: sreader,
                    wait: new_wait,
                    prod_wait: new_pwait,
                    op: op,
                })
            }
            Err(mreader) => {
                Err((op,
                     FuturesReceiver {
                         reader: mreader,
                         wait: new_wait,
                         prod_wait: new_pwait,
                     }))
            }
        }
    }

    /// Identical to Receiver::unsubscribe()
    pub fn unsubscribe(self) -> bool {
        self.reader.reader.get_consumers() == 1
    }
}

/// This struct acts as a SingleReceiver except operating as a futures Stream on incoming data
///
/// Since this operates in an iterator-like manner on the data stream, it holds the function
/// it calls and to use a different function must transform itself into a different
/// FuturesSingleReceiver using transform_operation
impl<R, F: FnMut(&T) -> R, T: Clone + Sync> FuturesSingleReceiver<R, F, T> {
    /// Identical to SingleReceiver::try_recv, uses the operation held by the FuturesSingleReceiver
    #[inline(always)]
    pub fn try_recv(&mut self) -> Result<R, TryRecvError> {
        let opref = &mut self.op;
        let rval = self.reader.try_recv_view(|tr| opref(tr));
        self.prod_wait.notify();
        rval.map_err(|x| x.1)
    }

    /// Adds another stream to the queue with a FuturesSingleReceiver using the passed function
    pub fn add_stream_with<Q, FQ: FnMut(&T) -> Q>(&self,
                                                  op: FQ)
                                                  -> FuturesSingleReceiver<Q, FQ, T> {
        let rx = self.reader.add_stream();
        FuturesSingleReceiver {
            reader: rx,
            wait: self.wait.clone(),
            prod_wait: self.prod_wait.clone(),
            op: op,
        }
    }

    /// This transforms the receiver into another FuturesSingleReceiver
    /// using a different function on the same stream
    pub fn transform_operation<Q, FQ: FnMut(&T) -> Q>(self,
                                                      op: FQ)
                                                      -> FuturesSingleReceiver<Q, FQ, T> {
        // Don't know how to satisy borrowck without absurd pointer lies
        // and forgetting shenanigans. Would rather pay the cost of add_stream for this
        self.add_stream_with(op)
    }

    /// Identical to Receiver::unsubscribe()
    pub fn unsubscribe(self) -> bool {
        self.reader.reader.reader.get_consumers() == 1
    }
}

//////// Futures stream/sink implementations

// The mpsc SendError struct can't be constructed according to rustc
// since it's a struct and the ctor is private. Copied and pasted here

/// Error type for sending, used when the receiving end of the channel is
/// dropped
pub struct SendError<T>(T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_tuple("SendError")
            .field(&"...")
            .finish()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "send failed because receiver is gone")
    }
}

impl<T> Error for SendError<T>
    where T: Any
{
    fn description(&self) -> &str {
        "send failed because receiver is gone"
    }
}

impl<T> SendError<T> {
    /// Returns the message that was attempted to be sent but failed.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: Clone> Sink for FuturesSender<T> {
    type SinkItem = T;
    type SinkError = SendError<T>;

    /// Essentially try_send except parks if the queue is full
    fn start_send(&mut self, msg: T) -> StartSend<T, SendError<T>> {

        match self.prod_wait.send_or_park(|m| self.writer.try_send(m), msg) {
            Ok(_) => {
                // see Sender::try_recv for why this isn't in the queue
                if self.writer.queue.needs_notify {
                    self.writer.queue.waiter.notify();
                }
                Ok(AsyncSink::Ready)
            }
            Err(TrySendError::Full(msg)) => Ok(AsyncSink::NotReady(msg)),
            Err(TrySendError::Disconnected(msg)) => Err(SendError(msg)),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), SendError<T>> {
        Ok(Async::Ready(()))
    }
}

impl<T: Clone> Stream for FuturesReceiver<T> {
    type Item = T;
    type Error = ();

    /// Essentially the same as recv
    fn poll(&mut self) -> Poll<Option<T>, ()> {
        self.reader.examine_signals();
        loop {
            match self.reader.queue.try_recv(&self.reader.reader) {
                Ok(msg) => {
                    self.prod_wait.notify();
                    return Ok(Async::Ready(Some(msg)));
                }
                Err((_, TryRecvError::Disconnected)) => return Ok(Async::Ready(None)),
                Err((pt, _)) => {
                    let count = self.reader.reader.load_count(Relaxed);
                    if unsafe { self.wait.fut_wait(count, &*pt, &self.reader.queue.writers) } {
                        return Ok(Async::NotReady);
                    }
                }
            }
        }
    }
}

impl<R, F: FnMut(&T) -> R, T: Clone + Sync> Stream for FuturesSingleReceiver<R, F, T> {
    type Item = R;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<R>, ()> {
        self.reader.reader.examine_signals();
        loop {
            let opref = &mut self.op;
            match self.reader
                .reader
                .queue
                .try_recv_view(|tr| opref(tr), &self.reader.reader.reader) {
                Ok(msg) => {
                    self.prod_wait.notify();
                    return Ok(Async::Ready(Some(msg)));
                }
                Err((_, _, TryRecvError::Disconnected)) => return Ok(Async::Ready(None)),
                Err((_, pt, _)) => {
                    let count = self.reader.reader.reader.load_count(Relaxed);
                    if unsafe {
                        self.wait.fut_wait(count, &*pt, &self.reader.reader.queue.writers)
                    } {
                        return Ok(Async::NotReady);
                    }
                }
            }
        }
    }
}


//////// FuturesWait

impl FuturesWait {
    pub fn new() -> FuturesWait {
        FuturesWait::with_spins(DEFAULT_TRY_SPINS, DEFAULT_YIELD_SPINS)
    }

    pub fn with_spins(spins_first: usize, spins_yield: usize) -> FuturesWait {
        FuturesWait {
            spins_first: spins_first,
            spins_yield: spins_yield,
            parked: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub fn fut_wait(&self, seq: usize, at: &AtomicUsize, wc: &AtomicUsize) -> bool {
        self.spin(seq, at, wc) && self.park(seq, at, wc)
    }

    pub fn spin(&self, seq: usize, at: &AtomicUsize, wc: &AtomicUsize) -> bool {
        for _ in 0..self.spins_first {
            if check(seq, at, wc) {
                return false;
            }
        }

        for _ in 0..self.spins_yield {
            yield_now();
            if check(seq, at, wc) {
                return false;
            }
        }
        return true;
    }

    pub fn park(&self, seq: usize, at: &AtomicUsize, wc: &AtomicUsize) -> bool {
        let mut parked = self.parked.lock();
        if check(seq, at, wc) {
            return false;
        }
        parked.push(park());
        return true;
    }

    fn send_or_park<T: Clone, F: Fn(T) -> Result<(), TrySendError<T>>>
        (&self,
         f: F,
         mut val: T)
         -> Result<(), TrySendError<T>> {
        for _ in 0..self.spins_first {
            match f(val) {
                Err(TrySendError::Full(v)) => val = v,
                v @ _ => return v,
            }
        }

        for _ in 0..self.spins_yield {
            yield_now();
            match f(val) {
                Err(TrySendError::Full(v)) => val = v,
                v @ _ => return v,
            }
        }

        let mut parked = self.parked.lock();
        match f(val) {
            Err(TrySendError::Full(v)) => {
                parked.push(park());
                return Err(TrySendError::Full(v));
            }
            v @ _ => return v,
        }
    }
}

impl Wait for FuturesWait {
    #[cold]
    fn wait(&self, _seq: usize, _w_pos: &AtomicUsize, _wc: &AtomicUsize) {
        assert!(false, "Somehow normal wait got called in futures queue");
    }

    fn notify(&self) {
        let mut parked = self.parked.lock();
        if parked.len() > 0 {
            if parked.len() > 8 {
                for val in parked.drain(..) {
                    val.unpark();
                }
            } else {
                let mut inline_v = smallvec::SmallVec::<[Task; 9]>::new();
                inline_v.extend(parked.drain(..));
                {
                    let _destruct = parked;
                }
                for val in inline_v.drain() {
                    val.unpark();
                }
            }
        }
    }

    fn needs_notify(&self) -> bool {
        true
    }
}

// Iterator Implemenatation

pub struct RecvIterator<T: Clone> {
    reader: Receiver<T>,
}

impl<T: Clone> IntoIterator for Receiver<T> {
    type Item = T;
    type IntoIter = RecvIterator<T>;

    fn into_iter(self) -> RecvIterator<T> {
        RecvIterator { reader: self }
    }
}

impl<T: Clone> Iterator for RecvIterator<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match self.reader.recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<T: Clone> RecvIterator<T> {
    pub fn get_recv(self) -> Receiver<T> {
        self.reader
    }
}

pub struct RecvPartialIterator<'a, T: 'a + Clone> {
    reader: &'a Receiver<T>,
}

impl<'a, T: 'a + Clone> Iterator for RecvPartialIterator<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match self.reader.try_recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

pub struct RecvViewIterator<R, F: FnMut(&T) -> R, T: Clone + Sync> {
    vals: (F, SingleReceiver<T>),
}

impl<R, F: FnMut(&T) -> R, T: Clone + Sync> Iterator for RecvViewIterator<R, F, T> {
    type Item = R;

    fn next(&mut self) -> Option<R> {
        let opref = &mut self.vals.0;
        match self.vals.1.recv_view(|v| opref(v)) {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<R, F: FnMut(&T) -> R, T: Clone + Sync> RecvViewIterator<R, F, T> {
    pub fn with_transform<Q, FQ: FnMut(&T) -> Q>(self, op: FQ) -> RecvViewIterator<Q, FQ, T> {
        let (_, myr) = self.vals;
        RecvViewIterator { vals: (op, myr) }
    }

    pub fn get_receiver(self) -> SingleReceiver<T> {
        self.vals.1
    }
}

pub struct RecvPartialViewIterator<'a, R, F: FnMut(&T) -> R, T: Clone + Sync + 'a> {
    vals: (F, &'a SingleReceiver<T>),
}

impl<'a, R, F: FnMut(&T) -> R, T: Clone + Sync + 'a> Iterator
    for RecvPartialViewIterator<'a, R, F, T> {
    type Item = R;

    fn next(&mut self) -> Option<R> {
        let opref = &mut self.vals.0;
        match self.vals.1.try_recv_view(|v| opref(v)) {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<'a, R, F: FnMut(&T) -> R, T: Clone + Sync + 'a> RecvPartialViewIterator<'a, R, F, T> {
    pub fn with_transform<Q, FQ: FnMut(&T) -> Q>(self,
                                                 op: FQ)
                                                 -> RecvPartialViewIterator<'a, Q, FQ, T> {
        let (_, myr) = self.vals;
        RecvPartialViewIterator { vals: (op, myr) }
    }
}


//////// Clone implementations

impl<T: Clone> Clone for Sender<T> {
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
    fn clone(&self) -> Sender<T> {
        self.state.set(QueueState::Multi);
        let rval = Sender {
            queue: self.queue.clone(),
            state: Cell::new(QueueState::Multi),
            token: self.queue.manager.get_token(),
        };
        self.queue.writers.fetch_add(1, SeqCst);
        rval
    }
}

impl<T: Clone> Clone for Receiver<T> {
    fn clone(&self) -> Receiver<T> {
        self.reader.dup_consumer();
        Receiver {
            queue: self.queue.clone(),
            reader: self.reader,
            token: self.queue.manager.get_token(),
            alive: true,
        }
    }
}

impl Clone for FuturesWait {
    fn clone(&self) -> FuturesWait {
        FuturesWait::with_spins(self.spins_first, self.spins_yield)
    }
}

//////// Drop implementations

impl<T: Clone> Drop for Sender<T> {
    fn drop(&mut self) {
        self.queue.writers.fetch_sub(1, SeqCst);
        fence(SeqCst);
        self.queue.manager.remove_token(self.token);
        self.queue.waiter.notify();
    }
}

impl<T: Clone> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.do_unsubscribe_with(|| ())
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

impl<T: Clone> Drop for FuturesReceiver<T> {
    fn drop(&mut self) {
        let prod_wait = self.prod_wait.clone();
        self.reader.do_unsubscribe_with(|| { prod_wait.notify(); })
    }
}

impl<R, F: FnMut(&T) -> R, T: Clone> Drop for FuturesSingleReceiver<R, F, T> {
    fn drop(&mut self) {
        let prod_wait = self.prod_wait.clone();
        self.reader.reader.do_unsubscribe_with(|| { prod_wait.notify(); })
    }
}


impl<T: Clone> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Multireader generic error message!")
    }
}

unsafe impl<T: Clone> Sync for MultiQueue<T> {}
unsafe impl<T: Clone> Send for MultiQueue<T> {}
unsafe impl<T: Send + Clone> Send for Sender<T> {}
unsafe impl<T: Send + Clone> Send for Receiver<T> {}
unsafe impl<T: Send + Clone> Send for SingleReceiver<T> {}

/// Creates a (Sender, Receiver) pair with a capacity that's
/// the next power of two >= the given capacity
///
/// # Example
/// ```
/// use multiqueue::multiqueue;
/// let (w, r) = multiqueue(10);
/// w.try_send(10).unwrap();
/// assert_eq!(10, r.try_recv().unwrap());
/// ```
pub fn multiqueue<T: Clone>(capacity: Index) -> (Sender<T>, Receiver<T>) {
    MultiQueue::new(capacity)
}

/// Creates a (Sender, Receiver) pair with a capacity that's
/// the next power of two >= the given capacity and the specified wait strategy
///
/// # Example
/// ```
/// use multiqueue::multiqueue_with;
/// use multiqueue::wait::BusyWait;
/// let (w, r) = multiqueue_with(10, BusyWait::new());
/// w.try_send(10).unwrap();
/// assert_eq!(10, r.try_recv().unwrap());
/// ```
pub fn multiqueue_with<T: Clone, W: Wait + 'static>(capacity: Index,
                                                    wait: W)
                                                    -> (Sender<T>, Receiver<T>) {
    MultiQueue::new_with(capacity, wait)
}

/// Creates a (FuturesSender, FuturesReceiver) pair witha  capacity
/// that's the next power of two >= the given capacity
///
/// # Example
/// ``'no_run
/// use multiqueue::futures_multiqueue;
/// extern crate futures;
/// use futures::stream::Stream;
/// use futures::sink::Sink;
///
/// let (fw, fr) = futures_multiqueue(10);
///
///
///
/// ```
pub fn futures_multiqueue<T: Clone>(capacity: Index) -> (FuturesSender<T>, FuturesReceiver<T>) {
    let cons_arc = Arc::new(FuturesWait::new());
    let prod_arc = Arc::new(FuturesWait::new());
    let (tx, rx) = MultiQueue::new_internal(capacity, cons_arc.clone());
    let ftx = FuturesSender {
        writer: tx,
        wait: cons_arc.clone(),
        prod_wait: prod_arc.clone(),
    };
    let rtx = FuturesReceiver {
        reader: rx,
        wait: cons_arc.clone(),
        prod_wait: prod_arc.clone(),
    };
    (ftx, rtx)
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
                let this_reader = reader.add_stream().into_single().unwrap();
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
        let reader_2 = reader.add_stream();
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
                let _this_reader = reader.add_stream();
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
            Dropper { aref: a }
        }
    }

    impl<'a> Drop for Dropper<'a> {
        fn drop(&mut self) {
            self.aref.fetch_sub(1, Ordering::Relaxed);
        }
    }

    impl<'a> Clone for Dropper<'a> {
        fn clone(&self) -> Dropper<'a> {
            self.aref.fetch_add(1, Ordering::Relaxed);
            Dropper { aref: self.aref }
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

    #[test]
    fn test_iterator_comp() {
        let (writer, reader) = MultiQueue::<usize>::new(10);
        drop(writer);
        for _ in reader {}
    }

    #[test]
    fn test_single_leav_multi() {
        let (writer, reader) = MultiQueue::new(10);
        let reader2 = reader.clone();
        writer.try_send(1).unwrap();
        writer.try_send(1).unwrap();
        assert_eq!(reader2.recv().unwrap(), 1);
        drop(reader2);
        let reader_s = reader.into_single().unwrap();
        assert!(reader_s.recv_view(|x| *x).is_ok());
    }
}
