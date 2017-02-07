use countedindex::Index;
use multiqueue::{InnerSend, InnerRecv, FutInnerSend, FutInnerRecv, FutInnerUniRecv, BCast,
                 MultiQueue, SendError, futures_multiqueue};
use wait::Wait;

use std::sync::mpsc::{TrySendError, TryRecvError, RecvError};

extern crate futures;
use self::futures::{Async, Poll, Sink, Stream, StartSend};

/// This class is the sending half of the broadcasting ```MultiQueue```. It supports both
/// single and multi consumer modes with competitive performance in each case.
/// It only supports nonblocking writes (the futures sender being an exception)
/// as well as being the conduit for adding new writers.
///
/// # Examples
///
/// ```
/// use std::thread;
///
/// let (send, recv) = multiqueue::broadcast_queue(4);
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
pub struct BroadcastSender<T: Clone> {
    sender: InnerSend<BCast<T>, T>,
}

/// This class is the receiving half of the broadcast ```MultiQueue```.
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
/// let (send, recv) = multiqueue::broadcast_queue(4);
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
pub struct BroadcastReceiver<T: Clone> {
    receiver: InnerRecv<BCast<T>, T>,
}


/// This class is similar to the receiver, except it ensures that there
/// is only one consumer for the stream it owns. This means that
/// one can safely view the data in-place with the recv_view method family
/// and avoid the cost of copying it. If there's only one receiver on a stream,
/// it can be converted into a ```BroadcastUniInnerRecv```
///
/// # Example:
///
/// ```
/// use multiqueue::broadcast_queue;
///
/// let (w, r) = broadcast_queue(10);
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
pub struct BroadcastUniReceiver<T: Clone + Sync> {
    receiver: InnerRecv<BCast<T>, T>,
}

/// This is the futures-compatible version of ```BroadcastSender```
/// It implements Sink
#[derive(Clone)]
pub struct BroadcastFutSender<T: Clone> {
    sender: FutInnerSend<BCast<T>, T>,
}

/// This is the futures-compatible version of ```BroadcastReceiver```
/// It implements ```Stream```
#[derive(Clone)]
pub struct BroadcastFutReceiver<T: Clone> {
    receiver: FutInnerRecv<BCast<T>, T>,
}

/// This is the futures-compatible version of ```BroadcastUniReceiver```
/// It implements ```Stream``` and behaves like the iterator would.
/// To use a different function must transform itself into a different
/// ```BroadcastFutUniRecveiver``` use ```transform_operation```
pub struct BroadcastFutUniReceiver<R, F: FnMut(&T) -> R, T: Clone + Sync> {
    receiver: FutInnerUniRecv<BCast<T>, R, F, T>,
}

impl<T: Clone> BroadcastSender<T> {
    #[inline(always)]
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        self.sender.try_send(val)
    }

    /// Removes the writer from the queue
    pub fn unsubscribe(self) {
        self.sender.unsubscribe();
    }
}

impl<T: Clone> BroadcastReceiver<T> {
    /// Tries to receive a value from the queue without blocking.
    ///
    /// # Examples:
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    /// let (w, r) = broadcast_queue(10);
    /// w.try_send(1).unwrap();
    /// assert_eq!(1, r.try_recv().unwrap());
    /// ```
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    /// use std::thread;
    ///
    /// let (send, recv) = broadcast_queue(10);
    ///
    /// let handle = thread::spawn(move || {
    ///     for _ in 0..10 {
    ///         loop {
    ///             match recv.try_recv() {
    ///                 Ok(val) => {
    ///                     println!("Got {}", val);
    ///                     break;
    ///                 },
    ///                 Err(_) => (),
    ///             }
    ///         }
    ///     }
    ///     assert!(recv.try_recv().is_err()); // recv would block here
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
        self.receiver.try_recv()
    }

    /// Receives a value from the queue, blocks until there is data.
    ///
    /// # Examples:
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    /// let (w, r) = broadcast_queue(10);
    /// w.try_send(1).unwrap();
    /// assert_eq!(1, r.recv().unwrap());
    /// ```
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    /// use std::thread;
    ///
    /// let (send, recv) = broadcast_queue(10);
    ///
    /// let handle = thread::spawn(move || {
    ///     // note the lack of dealing with failed reads.
    ///     // unwrap 'ignores' the error where sender disconnects
    ///     for _ in 0..10 {
    ///         println!("Got {}", recv.recv().unwrap());
    ///     }
    ///     assert!(recv.try_recv().is_err());
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
    pub fn recv(&self) -> Result<T, RecvError> {
        self.receiver.recv()
    }

    /// Adds a new data stream to the queue, starting at the same position
    /// as the ```BroadcastReceiver``` this is being called on.
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    /// let (w, r) = broadcast_queue(10);
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
    /// use multiqueue::broadcast_queue;
    ///
    /// use std::thread;
    ///
    /// let (send, recv) = broadcast_queue(4);
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
    pub fn add_stream(&self) -> BroadcastReceiver<T> {
        BroadcastReceiver { receiver: self.receiver.add_stream() }
    }

    /// Removes the given reader from the queue subscription lib
    /// Returns true if this is the last reader in a given broadcast unit
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    /// let (writer, reader) = broadcast_queue(1);
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
        self.receiver.unsubscribe()
    }

    /// Returns a non-owning iterator that iterates over the queue
    /// until it fails to receive an item, either through being empty
    /// or begin disconnected. This iterator will never block.
    ///
    /// # Examples:
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    /// let (w, r) = broadcast_queue(2);
    /// for _ in 0 .. 3 {
    ///     w.try_send(1).unwrap();
    ///     w.try_send(2).unwrap();
    ///     for val in r.try_iter().zip(1..2) {
    ///         assert_eq!(val.0, val.1);
    ///     }
    /// }
    /// ```
    pub fn try_iter<'a>(&'a self) -> BroadcastRefIter<'a, T> {
        BroadcastRefIter { recv: self }
    }
}

impl<T: Clone + Sync> BroadcastReceiver<T> {
    /// If there is only one ```BroadcastReceiver``` on the stream, converts the
    /// Receiver into a ```BroadcastUniReceiver``` otherwise returns the Receiver.
    ///
    /// # Example:
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    ///
    /// let (w, r) = broadcast_queue(10);
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
    pub fn into_single(self) -> Result<BroadcastUniReceiver<T>, BroadcastReceiver<T>> {
        if self.receiver.is_single() {
            Ok(BroadcastUniReceiver { receiver: self.receiver })
        } else {
            Err(self)
        }
    }
}

impl<T: Clone + Sync> BroadcastUniReceiver<T> {
    /// Identical to ```BroadcastReceiver::try_recv```
    #[inline(always)]
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Identical to ```BroadcastReceiver::recv```
    #[inline(always)]
    pub fn recv(&self) -> Result<T, RecvError> {
        self.receiver.recv()
    }

    /// Applies the passed function to the value in the queue without copying it out
    /// If there is no data in the queue or the writers have disconnected,
    /// returns an ```Err((F, TryRecvError))```
    ///
    /// # Example
    /// ```
    /// use multiqueue::broadcast_queue;
    ///
    /// let (w, r) = broadcast_queue(10);
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
        self.receiver.try_recv_view(op)
    }

    /// Applies the passed function to the value in the queue without copying it out
    /// If there is no data in the queue, blocks until an item is pushed into the queue
    /// or all writers disconnect
    ///
    /// # Example
    /// ```
    /// use multiqueue::broadcast_queue;
    ///
    /// let (w, r) = broadcast_queue(10);
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
        self.receiver.recv_view(op)
    }

    /// Almost identical to ```BroadcastReceiver::unsubscribe```, except it doesn't
    /// return a boolean of whether this was the last receiver on the stream
    /// because a receiver of this type must be the last one on the stream
    pub fn unsubscribe(self) {
        self.receiver.unsubscribe();
    }

    /// Transforms the ```BroadcastUniReceiver``` into a ```BroadcastReceiver```
    ///
    /// # Example
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    ///
    /// let (w, r) = broadcast_queue(10);
    /// w.try_send(1).unwrap();
    /// let single_r = r.into_single().unwrap();
    /// let normal_r = single_r.into_multi();
    /// normal_r.clone();
    /// ```
    pub fn into_multi(self) -> BroadcastReceiver<T> {
        BroadcastReceiver { receiver: self.receiver }
    }

    /// Returns a non-owning iterator that iterates over the queue
    /// until it fails to receive an item, either through being empty
    /// or begin disconnected. This iterator will never block.
    ///
    /// # Examples:
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    /// let (w, r) = broadcast_queue(2);
    /// let sr = r.into_single().unwrap();
    /// w.try_send(1).unwrap();
    /// w.try_send(2).unwrap();
    /// w.unsubscribe();
    /// for val in sr.iter_with(|x| 2 * *x).zip(1..2) {
    ///     assert_eq!(val.0, val.1 * 2);
    /// }
    /// ```
    pub fn iter_with<R, F: FnMut(&T) -> R>(self, op: F) -> BroadcastUniIter<R, F, T> {
        BroadcastUniIter {
            recv: self,
            op: op,
        }
    }

    /// Returns a non-owning iterator that iterates over the queue
    /// until it fails to receive an item, either through being empty
    /// or begin disconnected. This iterator will never block.
    ///
    /// # Examples:
    ///
    /// ```
    /// use multiqueue::broadcast_queue;
    /// let (w, r) = broadcast_queue(2);
    /// let sr = r.into_single().unwrap();
    /// for _ in 0 .. 3 {
    ///     w.try_send(1).unwrap();
    ///     w.try_send(2).unwrap();
    ///     for val in sr.try_iter_with(|x| 2 * *x).zip(1..2) {
    ///         assert_eq!(val.0, val.1*2);
    ///     }
    /// }
    /// ```
    pub fn try_iter_with<'a, R, F: FnMut(&T) -> R>(&'a self,
                                                   op: F)
                                                   -> BroadcastUniRefIter<'a, R, F, T> {
        BroadcastUniRefIter {
            recv: self,
            op: op,
        }
    }
}

impl<T: Clone> BroadcastFutSender<T> {
    /// Equivalent to ```BroadcastSender::try_send```
    #[inline(always)]
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        self.sender.try_send(val)
    }

    /// Equivalent to ```BroadcastSender::unsubscribe```
    pub fn unsubscribe(self) {
        self.sender.unsubscribe()
    }
}

impl<T: Clone> BroadcastFutReceiver<T> {
    /// Equivalent to ```BroadcastReceiver::try_recv```
    #[inline(always)]
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Equivalent to ```BroadcastReceiver::recv```
    #[inline(always)]
    pub fn recv(&self) -> Result<T, RecvError> {
        self.receiver.recv()
    }

    pub fn add_stream(&self) -> BroadcastFutReceiver<T> {
        BroadcastFutReceiver { receiver: self.receiver.add_stream() }
    }

    /// Identical to ```BroadcastReceiver::unsubscribe```
    pub fn unsubscribe(self) -> bool {
        self.receiver.unsubscribe()
    }
}

impl<T: Clone + Sync> BroadcastFutReceiver<T> {
    /// Analog of ```BroadcastReceiver::into_single```
    /// Since the ```BroadcastFutUniReceiver``` acts more like an iterator,
    /// this takes the operation to be applied to each value
    pub fn into_single<R, F: FnMut(&T) -> R>
        (self,
         op: F)
         -> Result<BroadcastFutUniReceiver<R, F, T>, (F, BroadcastFutReceiver<T>)> {
        match self.receiver.into_single(op) {
            Ok(sreceiver) => Ok(BroadcastFutUniReceiver { receiver: sreceiver }),
            Err((o, receiver)) => Err((o, BroadcastFutReceiver { receiver: receiver })),
        }
    }
}

impl<R, F: FnMut(&T) -> R, T: Clone + Sync> BroadcastFutUniReceiver<R, F, T> {
    /// Equivalent to ```BroadcastReceiver::try_recv``` using the held operation
    #[inline(always)]
    pub fn try_recv(&mut self) -> Result<R, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Equivalent to B```roadcastReceiver::recv``` using the held operation
    #[inline(always)]
    pub fn recv(&mut self) -> Result<R, RecvError> {
        self.receiver.recv()
    }

    /// Adds a stream with the specified method
    pub fn add_stream_with<RQ, FQ: FnMut(&T) -> RQ>(&self,
                                                    op: FQ)
                                                    -> BroadcastFutUniReceiver<RQ, FQ, T> {
        BroadcastFutUniReceiver { receiver: self.receiver.add_stream_with(op) }
    }

    /// Returns a new receiver on the same stream using a different method
    pub fn transform_operation<RQ, FQ: FnMut(&T) -> RQ>(self,
                                                        op: FQ)
                                                        -> BroadcastFutUniReceiver<RQ, FQ, T> {
        BroadcastFutUniReceiver { receiver: self.receiver.add_stream_with(op) }
    }

    /// Identical to ```BroadcastReceiver::unsubscribe```
    pub fn unsubscribe(self) -> bool {
        self.receiver.unsubscribe()
    }

    /// Transforms this back into ```BroadcastFutReceiver```, returning the new receiver
    pub fn into_multi(self) -> BroadcastFutReceiver<T> {
        BroadcastFutReceiver { receiver: self.receiver.into_multi() }
    }
}

impl<T: Clone> Sink for BroadcastFutSender<T> {
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

impl<T: Clone> Stream for BroadcastFutReceiver<T> {
    type Item = T;
    type Error = ();

    #[inline(always)]
    fn poll(&mut self) -> Poll<Option<T>, ()> {
        self.receiver.poll()
    }
}

impl<R, F: FnMut(&T) -> R, T: Clone + Sync> Stream for BroadcastFutUniReceiver<R, F, T> {
    type Item = R;
    type Error = ();

    #[inline(always)]
    fn poll(&mut self) -> Poll<Option<R>, ()> {
        self.receiver.poll()
    }
}

pub struct BroadcastIter<T: Clone> {
    recv: BroadcastReceiver<T>,
}

impl<T: Clone> Iterator for BroadcastIter<T> {
    type Item = T;

    #[inline(always)]
    fn next(&mut self) -> Option<T> {
        match self.recv.recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<T: Clone> IntoIterator for BroadcastReceiver<T> {
    type Item = T;

    type IntoIter = BroadcastIter<T>;

    fn into_iter(self) -> BroadcastIter<T> {
        BroadcastIter { recv: self }
    }
}

pub struct BroadcastSCIter<T: Clone + Sync> {
    recv: BroadcastUniReceiver<T>,
}

impl<T: Clone + Sync> Iterator for BroadcastSCIter<T> {
    type Item = T;

    #[inline(always)]
    fn next(&mut self) -> Option<T> {
        match self.recv.recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<T: Clone + Sync> IntoIterator for BroadcastUniReceiver<T> {
    type Item = T;

    type IntoIter = BroadcastSCIter<T>;

    fn into_iter(self) -> BroadcastSCIter<T> {
        BroadcastSCIter { recv: self }
    }
}


pub struct BroadcastRefIter<'a, T: Clone + 'a> {
    recv: &'a BroadcastReceiver<T>,
}

impl<'a, T: Clone + 'a> Iterator for BroadcastRefIter<'a, T> {
    type Item = T;

    #[inline(always)]
    fn next(&mut self) -> Option<T> {
        match self.recv.try_recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<'a, T: Clone + 'a> IntoIterator for &'a BroadcastReceiver<T> {
    type Item = T;

    type IntoIter = BroadcastRefIter<'a, T>;

    fn into_iter(self) -> BroadcastRefIter<'a, T> {
        BroadcastRefIter { recv: self }
    }
}

pub struct BroadcastSCRefIter<'a, T: Clone + Sync + 'a> {
    recv: &'a BroadcastUniReceiver<T>,
}

impl<'a, T: Clone + Sync + 'a> Iterator for BroadcastSCRefIter<'a, T> {
    type Item = T;

    #[inline(always)]
    fn next(&mut self) -> Option<T> {
        match self.recv.try_recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<'a, T: Clone + Sync + 'a> IntoIterator for &'a BroadcastUniReceiver<T> {
    type Item = T;

    type IntoIter = BroadcastSCRefIter<'a, T>;

    fn into_iter(self) -> BroadcastSCRefIter<'a, T> {
        BroadcastSCRefIter { recv: self }
    }
}


pub struct BroadcastUniIter<R, F: FnMut(&T) -> R, T: Clone + Sync> {
    recv: BroadcastUniReceiver<T>,
    op: F,
}

impl<R, F: FnMut(&T) -> R, T: Clone + Sync> Iterator for BroadcastUniIter<R, F, T> {
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

pub struct BroadcastUniRefIter<'a, R, F: FnMut(&T) -> R, T: Clone + Sync + 'a> {
    recv: &'a BroadcastUniReceiver<T>,
    op: F,
}

impl<'a, R, F: FnMut(&T) -> R, T: Clone + Sync + 'a> Iterator for BroadcastUniRefIter<'a, R, F, T> {
    type Item = R;

    #[inline(always)]
    fn next(&mut self) -> Option<R> {
        let opref = &mut self.op;
        match self.recv.try_recv_view(|v| opref(v)) {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

/// Creates a (```BroadcastSender```, ```BroadcastReceiver```) pair with a capacity that's
/// the next power of two >= the given capacity
///
/// # Example
/// ```
/// use multiqueue::broadcast_queue;
/// let (w, r) = broadcast_queue(10);
/// w.try_send(10).unwrap();
/// assert_eq!(10, r.try_recv().unwrap());
/// ```
pub fn broadcast_queue<T: Clone>(capacity: Index) -> (BroadcastSender<T>, BroadcastReceiver<T>) {
    let (send, recv) = MultiQueue::<BCast<T>, T>::new(capacity);
    (BroadcastSender { sender: send }, BroadcastReceiver { receiver: recv })
}

/// Creates a (```BroadcastSender```, ```BroadcastReceiver```) pair with a capacity that's
/// the next power of two >= the given capacity and the specified wait strategy
///
/// # Example
/// ```
/// use multiqueue::broadcast_queue_with;
/// use multiqueue::wait::BusyWait;
/// let (w, r) = broadcast_queue_with(10, BusyWait::new());
/// w.try_send(10).unwrap();
/// assert_eq!(10, r.try_recv().unwrap());
/// ```

pub fn broadcast_queue_with<T: Clone, W: Wait + 'static>
    (capacity: Index,
     wait: W)
     -> (BroadcastSender<T>, BroadcastReceiver<T>) {
    let (send, recv) = MultiQueue::<BCast<T>, T>::new_with(capacity, wait);
    (BroadcastSender { sender: send }, BroadcastReceiver { receiver: recv })
}

/// Futures variant of broadcast_queue - datastructures implement
/// Sink + Stream at a minor (~30 ns) performance cost to BlockingWait
pub fn broadcast_fut_queue<T: Clone>(capacity: Index)
                                     -> (BroadcastFutSender<T>, BroadcastFutReceiver<T>) {
    let (isend, irecv) = futures_multiqueue::<BCast<T>, T>(capacity);
    (BroadcastFutSender { sender: isend }, BroadcastFutReceiver { receiver: irecv })
}

unsafe impl<T: Send + Clone> Send for BroadcastSender<T> {}
unsafe impl<T: Send + Clone> Send for BroadcastReceiver<T> {}
unsafe impl<T: Send + Clone + Sync> Send for BroadcastUniReceiver<T> {}

#[cfg(test)]
mod test {

    use super::broadcast_queue;

    extern crate crossbeam;
    use self::crossbeam::scope;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::sync::mpsc::TryRecvError;
    use std::thread::yield_now;

    #[test]
    fn build_queue() {
        let _ = broadcast_queue::<usize>(10);
    }

    #[test]
    fn push_pop_test() {
        let (writer, reader) = broadcast_queue(1);
        for _ in 0..100 {
            assert!(reader.try_recv().is_err());
            writer.try_send(1 as usize).expect("Push should succeed");
            assert!(writer.try_send(1).is_err());
            assert_eq!(1, reader.try_recv().unwrap());
        }
    }

    fn mpsc_broadcast(senders: usize, receivers: usize) {
        let (writer, reader) = broadcast_queue(4);
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
        let (writer, reader) = broadcast_queue(1);
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
        let (writer, reader) = broadcast_queue(10);
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
        let (writer, reader) = broadcast_queue(1);
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
            let (writer, reader) = broadcast_queue(1);
            for _ in 0..10 {
                writer.try_send(Dropper::new(&count)).unwrap();
                reader.recv().unwrap();
            }
        }
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_iterator_comp() {
        let (writer, reader) = broadcast_queue::<usize>(10);
        drop(writer);
        for _ in reader {}
    }

    #[test]
    fn test_single_leave_multi() {
        let (writer, reader) = broadcast_queue::<usize>(10);
        let reader2 = reader.clone();
        writer.try_send(1).unwrap();
        writer.try_send(1).unwrap();
        assert_eq!(reader2.recv().unwrap(), 1);
        drop(reader2);
        let reader_s = reader.into_single().unwrap();
        assert!(reader_s.recv_view(|x| *x).is_ok());
    }
}
