use countedindex::Index;
use multiqueue::{InnerSend, InnerRecv, FutInnerSend, FutInnerRecv, FutInnerUniRecv, BCast,
                 MultiQueue, SendError, futures_multiqueue};
use wait::Wait;

use std::mem;
use std::sync::mpsc::{TrySendError, TryRecvError, RecvError};

extern crate futures;
use self::futures::{Async, Poll, Sink, Stream, StartSend};

/// This class is the sending half of the multicast Queue. It supports both
/// single and multi consumer modes with competitive performance in each case.
/// It only supports nonblocking writes (the futures sender being an exception)
/// as well as being the conduit for adding new writers.
///
/// # Examples
///
/// ```
/// use std::thread;
///
/// let (send, recv) = multiqueue::multicast_queue(4);
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
#[derive(Clone)]
pub struct MulticastSender<T: Clone> {
    sender: InnerSend<BCast<T>, T>,
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
/// let (send, recv) = multiqueue::multicast_queue(4);
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
#[derive(Clone, Debug)]
pub struct MulticastReceiver<T: Clone> {
    reader: InnerRecv<BCast<T>, T>,
}


/// This class is similar to the receiver, except it ensures that there
/// is only one consumer for the stream it owns. This means that
/// one can safely view the data in-place with the recv_view method family
/// and avoid the cost of copying it. If there's only one receiver on a stream,
/// it can be converted into a UniInnerRecv
///
/// # Example:
///
/// ```
/// use multiqueue::multicast_queue;
///
/// let (w, r) = multicast_queue(10);
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
pub struct MulticastUniReceiver<T: Clone + Sync> {
    reader: InnerRecv<BCast<T>, T>,
}

/// This is the futures-compatible version of MulticastSender
/// It implements Sink
#[derive(Clone)]
pub struct MulticastFutSender<T: Clone> {
    sender: FutInnerSend<BCast<T>, T>,
}

/// This is the futures-compatible version of MulticastReceiver
/// It implements Stream
#[derive(Clone)]
pub struct MulticastFutReceiver<T: Clone> {
    receiver: FutInnerRecv<BCast<T>, T>,
}

/// This is the futures-compatible version of MulticastUniReceiver
/// It implements Stream and behaves like the iterator would
pub struct MulticastFutUniReceiver<R, F: FnMut(&T) -> R, T: Clone + Sync> {
    receiver: FutInnerUniRecv<BCast<T>, R, F, T>,
}

impl<T: Clone> MulticastSender<T> {
    #[inline(always)]
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        self.sender.try_send(val)
    }

    pub fn unsubscribe(self) {
        self.sender.unsubscribe();
    }
}

impl<T: Clone> MulticastReceiver<T> {
    /// Tries to receive a value from the queue without blocking.
    ///
    /// # Examples:
    ///
    /// ```
    /// use multiqueue::multicast_queue;
    /// let (w, r) = multicast_queue(10);
    /// w.try_send(1).unwrap();
    /// assert_eq!(1, r.try_recv().unwrap());
    /// ```
    ///
    /// ```
    /// use multiqueue::multicast_queue;
    /// use std::thread;
    ///
    /// let (send, recv) = multicast_queue(10);
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
        self.reader.try_recv()
    }

    #[inline(always)]
    pub fn recv(&self) -> Result<T, RecvError> {
        self.reader.recv()
    }

    /// Adds a new data stream to the queue, starting at the same position
    /// as the InnerRecv this is being called on.
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::multicast_queue;
    /// let (w, r) = multicast_queue(10);
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
    /// use multiqueue::multicast_queue;
    ///
    /// use std::thread;
    ///
    /// let (send, recv) = multicast_queue(4);
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

    pub fn add_stream(&self) -> MulticastReceiver<T> {
        MulticastReceiver { reader: self.reader.add_stream() }
    }

    /// Removes the given reader from the queue subscription lib
    /// Returns true if this is the last reader in a given broadcast unit
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::multicast_queue;
    /// let (writer, reader) = multicast_queue(1);
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
        self.reader.unsubscribe()
    }
}

impl<T: Clone + Sync> MulticastReceiver<T> {
    pub fn into_single(self) -> Result<MulticastUniReceiver<T>, MulticastReceiver<T>> {
        if self.reader.is_single() {
            Ok(MulticastUniReceiver { reader: self.reader })
        } else {
            Err(self)
        }
    }
}

impl<T: Clone + Sync> MulticastUniReceiver<T> {
    /// Identical to MulticastReceiver::try_recv
    #[inline(always)]
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.reader.try_recv()
    }

    /// Identical to MulticastReceiver::recv
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
    /// use multiqueue::multicast_queue;
    ///
    /// let (w, r) = multicast_queue(10);
    /// let single_r = r.into_single().unwrap();
    /// for i in 0..5 {
    ///     w.try_send(i).unwrap();
    /// }
    ///
    /// for i in 0..5 {
    ///     let val = match single_r.try_recv_view(|x| 1 + *x) {
    ///         Ok(val) => val,
    ///         Err(_) => panic!("Queue shouldn't be disconncted or empty"),
    ///     };
    ///     assert_eq!(i + 1, val);
    /// }
    /// assert!(single_r.try_recv_view(|x| *x).is_err());
    /// drop(w);
    /// assert!(single_r.try_recv_view(|x| *x).is_err());
    /// ```
    #[inline(always)]
    pub fn try_recv_view<R, F: FnOnce(&T) -> R>(&self, op: F) -> Result<R, (F, TryRecvError)> {
        self.reader.try_recv_view(op)
    }

    /// Applies the passed function to the value in the queue without copying it out
    /// If there is no data in the queue, blocks until an item is pushed into the queue
    /// or all writers disconnect
    ///
    /// # Example
    /// ```
    /// use multiqueue::multicast_queue;
    ///
    /// let (w, r) = multicast_queue(10);
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
    pub fn recv_view<R, F: FnOnce(&T) -> R>(&self, op: F) -> Result<R, (F, RecvError)> {
        self.reader.recv_view(op)
    }

    /// Almost identical to MulticastReceiver::unsubscribe, except it doesn't
    /// return a boolean of whether this was the last receiver on the stream
    /// because a receiver of this type must be the last one on the stream
    pub fn unsubscribe(self) {
        self.reader.unsubscribe();
    }

    pub fn iter_with<R, F: FnMut(&T) -> R>(self, op: F) -> MulticastUniIter<R, F, T> {
        MulticastUniIter {
            recv: self,
            op: op,
        }
    }

    pub fn partial_iter_with<'a, R, F: FnMut(&T) -> R>(&'a self,
                                                       op: F)
                                                       -> MulticastUniRefIter<'a, R, F, T> {
        MulticastUniRefIter {
            recv: self,
            op: op,
        }
    }
}

/// Creates a (MulticastSender, MulticastReceiver) pair with a capacity that's
/// the next power of two >= the given capacity
///
/// # Example
/// ```
/// use multiqueue::multicast_queue;
/// let (w, r) = multicast_queue(10);
/// w.try_send(10).unwrap();
/// assert_eq!(10, r.try_recv().unwrap());
/// ```
pub fn multicast_queue<T: Clone>(capacity: Index) -> (MulticastSender<T>, MulticastReceiver<T>) {
    let (send, recv) = MultiQueue::<BCast<T>, T>::new(capacity);
    (MulticastSender { sender: send }, MulticastReceiver { reader: recv })
}

/// Creates a (MulticastSender, MulticastReceiver) pair with a capacity that's
/// the next power of two >= the given capacity and the specified wait strategy
///
/// # Example
/// ```
/// use multiqueue::multicast_queue_with;
/// use multiqueue::wait::BusyWait;
/// let (w, r) = multicast_queue_with(10, BusyWait::new());
/// w.try_send(10).unwrap();
/// assert_eq!(10, r.try_recv().unwrap());
/// ```

pub fn multicast_queue_with<T: Clone, W: Wait + 'static>
    (capacity: Index,
     wait: W)
     -> (MulticastSender<T>, MulticastReceiver<T>) {
    let (send, recv) = MultiQueue::<BCast<T>, T>::new_with(capacity, wait);
    (MulticastSender { sender: send }, MulticastReceiver { reader: recv })
}

/// Futures variant of multicast_queue - datastructures implement
/// Sink + Stream at a minor (~30 ns) performance cost to BlockingWait
pub fn multicast_fut_queue<T: Clone>(capacity: Index)
                                     -> (MulticastFutSender<T>, MulticastFutReceiver<T>) {
    let (isend, irecv) = futures_multiqueue::<BCast<T>, T>(capacity);
    (MulticastFutSender { sender: isend }, MulticastFutReceiver { receiver: irecv })
}

unsafe impl<T: Send + Clone> Send for MulticastSender<T> {}
unsafe impl<T: Send + Clone> Send for MulticastReceiver<T> {}
unsafe impl<T: Send + Clone + Sync> Send for MulticastUniReceiver<T> {}

pub struct MulticastIter<T: Clone> {
    recv: MulticastReceiver<T>,
}

impl<T: Clone> Iterator for MulticastIter<T> {
    type Item = T;

    #[inline(always)]
    fn next(&mut self) -> Option<T> {
        match self.recv.recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<T: Clone> IntoIterator for MulticastReceiver<T> {
    type Item = T;

    type IntoIter = MulticastIter<T>;

    fn into_iter(self) -> MulticastIter<T> {
        MulticastIter { recv: self }
    }
}

pub struct MulticastRefIter<'a, T: Clone + 'a> {
    recv: &'a MulticastReceiver<T>,
}

impl<'a, T: Clone + 'a> Iterator for MulticastRefIter<'a, T> {
    type Item = T;

    #[inline(always)]
    fn next(&mut self) -> Option<T> {
        match self.recv.try_recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

pub struct MulticastUniIter<R, F: FnMut(&T) -> R, T: Clone + Sync> {
    recv: MulticastUniReceiver<T>,
    op: F,
}

impl<R, F: FnMut(&T) -> R, T: Clone + Sync> Iterator for MulticastUniIter<R, F, T> {
    type Item = R;

    #[inline(always)]
    fn next(&mut self) -> Option<R> {
        let opref = &mut self.op;
        match self.recv.recv_view(|v| opref(v)) {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

pub struct MulticastUniRefIter<'a, R, F: FnMut(&T) -> R, T: Clone + Sync + 'a> {
    recv: &'a MulticastUniReceiver<T>,
    op: F,
}

impl<'a, R, F: FnMut(&T) -> R, T: Clone + Sync + 'a> Iterator for MulticastUniRefIter<'a, R, F, T> {
    type Item = R;

    #[inline(always)]
    fn next(&mut self) -> Option<R> {
        let opref = &mut self.op;
        match self.recv.recv_view(|v| opref(v)) {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<T: Clone> MulticastFutSender<T> {
    /// Equivalent to MulticastSender::try_send
    #[inline(always)]
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        self.sender.try_send(val)
    }

    /// Equivalent to MulticastSender::unsubscribe
    pub fn unsubscribe(self) {
        self.sender.unsubscribe()
    }
}

impl<T: Clone> MulticastFutReceiver<T> {
    /// Equivalent to MulticastReceiver::try_recv
    #[inline(always)]
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Equivalent to MulticastReceiver::recv
    #[inline(always)]
    pub fn recv(&self) -> Result<T, RecvError> {
        self.receiver.recv()
    }

    pub fn add_stream(&self) -> MulticastFutReceiver<T> {
        MulticastFutReceiver { receiver: self.receiver.add_stream() }
    }

    /// Identical to MulticastReceiver::unsubscribe
    pub fn unsubscribe(self) -> bool {
        self.receiver.unsubscribe()
    }
}

impl<R, F: FnMut(&T) -> R, T: Clone + Sync> MulticastFutUniReceiver<R, F, T> {
    /// Equivalent to MulticastReceiver::try_recv using the held operation
    #[inline(always)]
    pub fn try_recv(&mut self) -> Result<R, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Equivalent to MulticastReceiver::recv using the held operation
    #[inline(always)]
    pub fn recv(&mut self) -> Result<R, RecvError> {
        self.receiver.recv()
    }

    pub fn add_stream_with<RQ, FQ: FnMut(&T) -> RQ>(&self,
                                                    op: FQ)
                                                    -> MulticastFutUniReceiver<RQ, FQ, T> {
        MulticastFutUniReceiver { receiver: self.receiver.add_stream_with(op) }
    }

    /// Identical to MulticastReceiver::unsubscribe
    pub fn unsubscribe(self) -> bool {
        self.receiver.unsubscribe()
    }
}

impl<T: Clone> Sink for MulticastFutSender<T> {
    type SinkItem = T;
    type SinkError = SendError<T>;

    #[inline(always)]
    fn start_send(&mut self, msg: T) -> StartSend<T, SendError<T>> {
        self.sender.start_send(msg)
    }

    #[inline(always)]
    fn poll_complete(&mut self) -> Poll<(), SendError<T>> {
        Ok(Async::Ready(()))
    }
}

impl<T: Clone> Stream for MulticastFutReceiver<T> {
    type Item = T;
    type Error = ();

    #[inline(always)]
    fn poll(&mut self) -> Poll<Option<T>, ()> {
        self.receiver.poll()
    }
}

impl<R, F: FnMut(&T) -> R, T: Clone + Sync> Stream for MulticastFutUniReceiver<R, F, T> {
    type Item = R;
    type Error = ();

    #[inline(always)]
    fn poll(&mut self) -> Poll<Option<R>, ()> {
        self.receiver.poll()
    }
}

#[cfg(test)]
mod test {

    use super::multicast_queue;

    extern crate crossbeam;
    use self::crossbeam::scope;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::sync::mpsc::TryRecvError;
    use std::thread::yield_now;

    #[test]
    fn build_queue() {
        let _ = multicast_queue::<usize>(10);
    }

    #[test]
    fn push_pop_test() {
        let (writer, reader) = multicast_queue(1);
        for _ in 0..100 {
            assert!(reader.try_recv().is_err());
            writer.try_send(1 as usize).expect("Push should succeed");
            assert!(writer.try_send(1).is_err());
            assert_eq!(1, reader.try_recv().unwrap());
        }
    }

    fn mpsc_broadcast(senders: usize, receivers: usize) {
        let (writer, reader) = multicast_queue(4);
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
        let (writer, reader) = multicast_queue(1);
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
        let (writer, reader) = multicast_queue(10);
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
        let (writer, reader) = multicast_queue(1);
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
            let (writer, reader) = multicast_queue(1);
            for _ in 0..10 {
                writer.try_send(Dropper::new(&count)).unwrap();
                reader.recv().unwrap();
            }
        }
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_iterator_comp() {
        let (writer, reader) = multicast_queue::<usize>(10);
        drop(writer);
        for _ in reader {}
    }

    #[test]
    fn test_single_leave_multi() {
        let (writer, reader) = multicast_queue::<usize>(10);
        let reader2 = reader.clone();
        writer.try_send(1).unwrap();
        writer.try_send(1).unwrap();
        assert_eq!(reader2.recv().unwrap(), 1);
        drop(reader2);
        let reader_s = reader.into_single().unwrap();
        assert!(reader_s.recv_view(|x| *x).is_ok());
    }
}
