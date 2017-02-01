//! This crate provides a fast mpmc multicast queue.
//! It's based on the queue design from the LMAX Disruptor, with a few improvements:
//!
//!   * It acts as a futures stream/sink, so you can set up high-performance pipelines
//!
//!   * It can dynamically add/remove senders, and each stream can have multiple receivers
//!
//!   * It has fast runtime fallbacks for whenr there's a single consumer and/or a single producer
//!
//!   * It works on 32 bit systems without any performance or capability penalty
//!
//!   * In most cases, one can view data written directly into the queue without copying it
//!
//! In many cases, MultiQueue will be a good replacement for channels and it's broadcast
//! capabilities can replace more complex concurrency systems with a single queue.
//!
//! #Queue Model:
//! MultiQueue functions similarly to the LMAX Disruptor from a high level view.
//! There's an incoming FIFO data stream that is broadcast to a set of subscribers
//! as if there were multiple streams being written to.
//! There are two main differences:
//!   * MultiQueue transparently supports switching between single and multiple producers.
//!   * Each broadcast stream can be shared among multiple consumers.
//!
//! The last part makes the model a bit confusing, since there's a difference between a
//! stream of data and something consuming that stream. To make things worse, each consumer
//! may not actually see each value on the stream. Instead, multiple consumers may act on
//! a single stream each getting unique access to certain elements.
//!
//! A helpful mental model may be to think about this as if each stream was really just an mpmc
//! queue that was getting pushed to, and the MultiQueue structure just assembled a bunch
//! together behind the scenes. This isn't the case of course, but it's helpful for thinking.
//!
//! An diagram that represents a general use case of the queue where each consumer has unique
//! access to a stream is below - the # stand in for producers and @ stands in for the consumer of
//! each stream, each with a label. The lines are meant to show the data flow through the queue.
//!
//! ```text
//! -> #        @-1
//!     \      /
//!      -> -> -> @-2
//!     /      \
//! -> #        @-3
//! ```
//!
//! This is a pretty standard broadcast queue setup -
//! for each element sent in, it is seen on each stream by that's streams consumer.
//!
//!
//! However, in MultiQueue, each logical consumer might actually be demultiplexed
//! across many actual consumers, like below.
//!
//! ```text
//! -> #        @-1
//!     \      /
//!      -> -> -> @-2' (really @+@+@ each compete for a spot)
//!     /      \
//! -> #        @-3
//! ```
//!
//! If this diagram is redrawn with each of the producers sending in a
//! sequenced element (time goes left  to right):
//!
//! ```text
//! t=1|t=2|    t=3    | t=4|
//! 1 -> #              @-1 (1, 2)
//!       \            /
//!        -> 2 -> 1 -> -> @-2' (really @ (1) + @ (2) + @ (nothing yet))
//!       /            \
//! 2 -> #              @-3 (1, 2)
//!```
//!
//! If one imagines this as a webserver, the streams for @-1 and @-3 might be doing random
//! webservery work like some logging or metrics gathering and can handle
//! the workload completely on one core, @-2 is doing expensive work handling requests
//! and is split into multiple workers dealing with the data stream.
//!
//!
//! #Usage:
//! From the receiving side, this behaves quite similarly to a channel receiver.
//! The .recv function will block until data is available and then return the data.
//!
//! For senders, there is only .try_send (except for the futures sink, which can park),
//! This is due to performance and api reasons - you should handle backlog instead of just blocking.
//!
//! # Example: SPSC channel
//!
//! ```
//! extern crate multiqueue;
//!
//! use std::thread;
//!
//! let (send, recv) = multiqueue::multiqueue(10);
//!
//! let handle = thread::spawn(move || {
//!     for val in recv {
//!         println!("Got {}", val);
//!     }
//! });
//!
//! for i in 0..10 {
//!     send.try_send(i).unwrap();
//! }
//!
//! // Drop the sender to close the queue
//! drop(send);
//!
//! handle.join();
//!
//! // prints
//! // Got 0
//! // Got 1
//! // Got 2
//! // etc
//! ```
//!
//! # Example: SPSC broadcasting
//!
//! ```
//! extern crate multiqueue;
//!
//! use std::thread;
//!
//! let (send, recv) = multiqueue::multiqueue(4);
//! let mut handles = vec![];
//! for i in 0..2 { // or n
//!     let cur_recv = recv.add_stream();
//!     handles.push(thread::spawn(move || {
//!         for val in cur_recv {
//!             println!("Stream {} got {}", i, val);
//!         }
//!     }));
//! }
//!
//! // Take notice that I drop the reader - this removes it from
//! // the queue, meaning that the readers in the new threads
//! // won't get starved by the lack of progress from recv
//! recv.unsubscribe();
//!
//! for i in 0..10 {
//!     // Don't do this busy loop in real stuff unless you're really sure
//!     loop {
//!         if send.try_send(i).is_ok() {
//!             break;
//!         }
//!     }
//! }
//!
//! // Drop the sender to close the queue
//! drop(send);
//!
//! for t in handles {
//!     t.join();
//! }
//!
//! // prints along the lines of
//! // Stream 0 got 0
//! // Stream 0 got 1
//! // Stream 1 got 0
//! // Stream 0 got 2
//! // Stream 1 got 1
//! // etc
//!
//! ```
//!
//! //! # Example: SPMC broadcast
//!
//! ```
//! extern crate multiqueue;
//!
//! use std::thread;
//!
//! let (send, recv) = multiqueue::multiqueue(4);
//!
//! let mut handles = vec![];
//!
//! for i in 0..2 { // or n
//!     let cur_recv = recv.add_stream();
//!     for j in 0..2 {
//!         let stream_consumer = cur_recv.clone();
//!         handles.push(thread::spawn(move || {
//!             for val in stream_consumer {
//!                 println!("Stream {} consumer {} got {}", i, j, val);
//!             }
//!         }));
//!     }
//!     // cur_recv is dropped here
//! }
//!
//! // Take notice that I drop the reader - this removes it from
//! // the queue, meaning that the readers in the new threads
//! // won't get starved by the lack of progress from recv
//! recv.unsubscribe();
//!
//! for i in 0..10 {
//!     // Don't do this busy loop in real stuff unless you're really sure
//!     loop {
//!         if send.try_send(i).is_ok() {
//!             break;
//!         }
//!     }
//! }
//! drop(send);
//!
//! for t in handles {
//!     t.join();
//! }
//!
//! // prints along the lines of
//! // Stream 0 consumer 1 got 2
//! // Stream 0 consumer 0 got 0
//! // Stream 1 consumer 0 got 0
//! // Stream 0 consumer 1 got 1
//! // Stream 1 consumer 1 got 1
//! // Stream 1 consumer 0 got 2
//! // etc
//!
//! // some join mechanics here
//! ```
//!
//! # Example: Usage menagerie
//!
//! ```
//! extern crate multiqueue;
//!
//! use std::thread;
//!
//! let (send, recv) = multiqueue::multiqueue(4);
//! let mut handles = vec![];
//!
//! // start like before
//! for i in 0..2 { // or n
//!     let cur_recv = recv.add_stream();
//!     for j in 0..2 {
//!         let stream_consumer = cur_recv.clone();
//!         handles.push(thread::spawn(move ||
//!             for val in stream_consumer {
//!                 println!("Stream {} consumer {} got {}", i, j, val);
//!             }
//!         ));
//!     }
//!     // cur_recv is dropped here
//! }
//!
//! // On this stream, since there's only one consumer,
//! // the receiver can be made into a SingleReceiver
//! // which can view items inline in the queue
//! let single_recv = recv.add_stream().into_single().unwrap();
//!
//! handles.push(thread::spawn(move ||
//!     for val in single_recv.iter_with(|item_ref| 10 * *item_ref) {
//!         println!("{}", val);
//!     }
//! ));
//!
//! // Same as above, except this time we just want to iterate until the receiver is empty
//! let single_recv_2 = recv.add_stream().into_single().unwrap();
//!
//! handles.push(thread::spawn(move ||
//!     for val in single_recv_2.partial_iter_with(|item_ref| 10 * *item_ref) {
//!         println!("{}", val);
//!     }
//! ));
//!
//! // Take notice that I drop the reader - this removes it from
//! // the queue, meaning that the readers in the new threads
//! // won't get starved by the lack of progress from recv
//! recv.unsubscribe();
//!
//! // Many senders to give all the receivers something
//! for _ in 0..3 {
//!     let cur_send = send.clone();
//!     handles.push(thread::spawn(move ||
//!         for i in 0..10 {
//!             loop {
//!                 if cur_send.try_send(i).is_ok() {
//!                     break;
//!                 }
//!             }
//!         }
//!     ));
//! }
//! drop(send);
//!
//! for t in handles {
//!    t.join();
//! }
//! ```

mod alloc;
mod atomicsignal;
mod consume;
mod countedindex;
mod maybe_acquire;
mod memory;
mod multiqueue;
mod read_cursor;
pub mod wait;

use multiqueue::{InnerSend, InnerRecv, BCast, MPMC, MultiQueue};
use countedindex::Index;

use std::sync::mpsc::{TrySendError, TryRecvError, RecvError};

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
/// it can be converted into a SingleInnerRecv
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
pub struct SingleBcastReceiver<T: Clone + Sync> {
    reader: InnerRecv<BCast<T>, T>,
}

/// This is the receiving end of a standard mpmc view of the queue
/// It functions similarly to the broadcast queue execpt there
/// is only ever one stream. As a result, the type doesn't need to be clone
#[derive(Clone, Error)]
pub struct MPMCReceiver<T> {
    reader: InnerRecv<MPMC<T>, T>,
}

pub struct SingleMPMCReceiver<T> {
    reader: InnerRecv<MPMC<T>, T>,
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

    pub fn add_stream(&self) -> MulticastReceiver<T> {
        MulticastReceiver {
            reader: reader.add_stream(),
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
        self.reader.unsubscribe()
    }
}

impl<T> MPMCReceiver<T> {
    /// Tries to receive a value from the queue without blocking.
    ///
    /// # Examples:
    ///
    /// ```
    /// use multiqueue::mpmc_queue;
    /// let (w, r) = mpmc_queue(10);
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

    /// Removes the given reader from the queue subscription lib
    /// Returns true if this is the last reader in a given broadcast unit
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::multiqueue;
    /// let (writer, reader) = multiqueue(2);
    /// writer.try_send(1).expect("This will succeed since queue is empty");
    /// reader.try_recv().expect("This reader can read");
    /// reader.unsubscribe();
    /// // Fails since there's no readers left
    /// assert!(writer.try_send(1).is_err());
    /// ```
    pub fn unsubscribe(self) -> bool {
        self.reader.unsubscribe()
    }
}

/*
/// If there is only one InnerRecv on the stream, converts the
/// InnerRecv into a SingleInnerRecv otherwise returns the InnerRecv.
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
//    pub fn into_single(&self) -> Result<Receiver<T>, Sender<T>> {
//
//   }
 */

impl<T> SingleMPMCReceiver<T> {

    /// Identical to MPMCReceiver::try_recv
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.reader.try_recv()
    }

    /// Identical to MPMCReceiver::recv
    pub fn recv(&self) -> Result<T, RecvError> {
        self.reader.recv()
    }


    /// Similar to SingleMcastReceiver::try_recv_view, except this closure takes
    /// a mutable reference to the data
    pub fn try_recv_view<R, F: FnOnce(&mut T) -> R>(op: F) -> Result<R, (F, TryRecvError)> {
        self.reader.try_recv_view(op)
    }

    /// Similar to SingleMcastReceiver::recv_view, except this closure takes
    /// a mutable reference to the data
    pub fn try_recv_view<R, F: FnOnce(&mut T) -> T>(op: F) -> Result<R, (F, RecvError)> {
        self.reader.recv_view(op)
    }

    /// Removes the given reader from the queue subscription lib
    /// Returns true if this is the last reader in a given broadcast unit
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::multiqueue;
    /// let (writer, reader) = multiqueue(2);
    /// writer.try_send(1).expect("This will succeed since queue is empty");
    /// reader.try_recv().expect("This reader can read");
    /// reader.unsubscribe();
    /// // Fails since there's no readers left
    /// assert!(writer.try_send(1).is_err());
    /// ```
    pub fn unsubscribe(self) -> bool {
        self.reader.unsubscribe()
    }
}

pub fn multicast_queue<T: Clone>(capacity: Index) -> (MulticastSender<T>, MulticastReceiver<T>) {
    let (send, recv) = MultiQueue::<BCast<T>, T>::new(capacity);
    (MulticastSender { sender: send }, MulticastReceiver { reader: recv })
}

pub fn mpmc_queue<T>(capacity: Index) -> (MPMCSender, MPMCReceiver) {
    let (send, recv) = MultiQueue::<BCast<T>, T>::new(capacity);
    (MPMCSender { sender: send }, MPMCReceiver { reader: recv })
}