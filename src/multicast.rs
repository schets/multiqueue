use multiqueue::{InnerSend, InnerRecv, BCast, Multicast, MultiQueue};
use countedindex::Index;

use std::sync::mpsc::{TrySendError, TryRecvError, RecvError};


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
#[derive(Clone)]
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
pub struct MulticastUniReceiver<T: Clone + Sync> {
    reader: InnerRecv<BCast<T>, T>,
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
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.reader.try_recv()
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        self.reader.recv()
    }

    /// Adds a new data stream to the queue, starting at the same position
    /// as the InnerRecv this is being called on.
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

    pub fn add_stream(&self) -> MulticastReceiver<T> {
        MulticastReceiver { reader: self.reader.add_stream() }
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
        self.reader.unsubscribe()
    }
}

impl<T: Clone + Sync> MulticastUniReceiver<T> {
    /// Identical to MulticastReceiver::try_recv
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.reader.try_recv()
    }

    /// Identical to MulticastReceiver::recv
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
    /// let (w, r) = multicase_queue(10);
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
    pub fn try_recv_view<R, F: FnOnce(&T) -> R>(&self, op: F) -> Result<R, (F, TryRecvError)> {
        let mut_w = |v: &mut T| op(v);
        self.reader.try_recv_view(mut_w)
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
    pub fn recv_view<R, F: FnOnce(&T) -> R>(&self, op: F) -> Result<R, (F, RecvError)> {
        let mut_w = |v: &mut T| op(v);
        self.reader.recv_view(mut_w)
    }

    /// Almost identical to MulticastReceiver::unsubscribe, except it doesn't
    /// return a boolean of whether this was the last receiver on the stream
    /// because a receiver of this type must be the last one on the stream
    pub fn unsubscribe(self) {
        self.reader.unsubscribe();
    }
}

pub fn multicast_queue<T: Clone>(capacity: Index) -> (MulticastSender<T>, MulticastReceiver<T>) {
    let (send, recv) = MultiQueue::<BCast<T>, T>::new(capacity);
    (MulticastSender { sender: send }, MulticastReceiver { reader: recv })
}

unsafe impl<T: Send + Clone> Send for MulticastSender<T> {}
unsafe impl<T: Send + Clone> Send for MulticastReceiver<T> {}
unsafe impl<T: Send + Clone + Sync> Send for MulticastUniReceiver<T> {}

pub struct MulticastIter<T: Clone> {
    recv: MulticastReceiver<T>,
}

impl<T: Clone> Iterator for MulticastIter<T> {
    type Item = T;

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
