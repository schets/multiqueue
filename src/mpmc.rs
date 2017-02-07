use countedindex::Index;
use multiqueue::{InnerSend, InnerRecv, MPMC, MultiQueue, FutInnerSend, FutInnerRecv,
                 FutInnerUniRecv, SendError, futures_multiqueue};
use wait::Wait;

use std::sync::mpsc::{TrySendError, TryRecvError, RecvError};

extern crate futures;
use self::futures::{Async, Poll, Sink, Stream, StartSend};

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
/// let (send, recv) = multiqueue::mpmc_queue(4);
///
/// let mut handles = vec![];
///
/// for i in 0..2 { // or n
///     let consumer = recv.clone();
///     handles.push(thread::spawn(move || {
///         for val in consumer {
///             println!("Consumer {} got {}", i, val);
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
/// drop(send);
///
/// for t in handles {
///     t.join();
/// }
/// // prints along the lines of
/// // Consumer 1 got 2
/// // Consumer 0 got 0
/// // Consumer 0 got 1
/// // etc
/// ```
#[derive(Clone)]
pub struct MPMCSender<T> {
    sender: InnerSend<MPMC<T>, T>,
}


/// This is the receiving end of a standard mpmc view of the queue
/// It functions similarly to the broadcast queue execpt there
/// is only ever one stream. As a result, the type doesn't need to be clone
#[derive(Clone, Debug)]
pub struct MPMCReceiver<T> {
    reader: InnerRecv<MPMC<T>, T>,
}


/// This is the receiving end of a standard mpmc view of the queue
/// for when it's statically know that there is only one receiver.
/// It functions similarly to the broadcast queue UniReceiver execpt there
/// is only ever one stream. As a result, the type doesn't need to be clone or sync
pub struct MPMCUniReceiver<T> {
    reader: InnerRecv<MPMC<T>, T>,
}

/// This is the futures-compatible version of MPMCSender
/// It implements Sink
#[derive(Clone)]
pub struct MPMCFutSender<T> {
    sender: FutInnerSend<MPMC<T>, T>,
}

/// This is the futures-compatible version of MPMCReceiver
/// It implements Stream
#[derive(Clone)]
pub struct MPMCFutReceiver<T> {
    receiver: FutInnerRecv<MPMC<T>, T>,
}

/// This is the futures-compatible version of MPMCUniReceiver
/// It implements Stream and behaves like the iterator would
pub struct MPMCFutUniReceiver<R, F: FnMut(&T) -> R, T> {
    receiver: FutInnerUniRecv<MPMC<T>, R, F, T>,
}

impl<T> MPMCSender<T> {
    /// Tries to send a value into the queue
    /// If there is no space, returns Err(TrySendError::Full(val))
    /// If there are no readers, returns Err(TrySendError::Disconnected(val))
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        self.sender.try_send(val)
    }

    /// Removes this writer from the queue
    pub fn unsubscribe(self) {
        self.sender.unsubscribe()
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
    /// use multiqueue::mpmc_queue;
    /// use std::thread;
    ///
    /// let (send, recv) = mpmc_queue(10);
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
    /// use multiqueue::mpmc_queue;
    /// let (writer, reader) = mpmc_queue(2);
    /// writer.try_send(1).expect("This will succeed since queue is empty");
    /// reader.try_recv().expect("This reader can read");
    /// reader.unsubscribe();
    /// // Fails since there's no readers left
    /// assert!(writer.try_send(1).is_err());
    /// ```
    pub fn unsubscribe(self) -> bool {
        self.reader.unsubscribe()
    }

    pub fn into_single(self) -> Result<MPMCUniReceiver<T>, MPMCReceiver<T>> {
        if self.reader.is_single() {
            Ok(MPMCUniReceiver { reader: self.reader })
        } else {
            Err(self)
        }
    }
}

/*
/// If there is only one InnerRecv on the stream, converts the
/// InnerRecv into a UniInnerRecv otherwise returns the InnerRecv.
///
/// # Example:
///
/// ```
/// use multiqueue::mpmc_queue;
///
/// let (w, r) = mpmc_queue(10);
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

impl<T> MPMCUniReceiver<T> {
    /// Identical to MPMCReceiver::try_recv
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.reader.try_recv()
    }

    /// Identical to MPMCReceiver::recv
    pub fn recv(&self) -> Result<T, RecvError> {
        self.reader.recv()
    }


    /// Similar to UniMcastReceiver::try_recv_view, except this closure takes
    pub fn try_recv_view<R, F: FnOnce(&T) -> R>(&self, op: F) -> Result<R, (F, TryRecvError)> {
        self.reader.try_recv_view(op)
    }

    /// Similar to UniMcastReceiver::recv_view
    pub fn recv_view<R, F: FnOnce(&T) -> R>(&self, op: F) -> Result<R, (F, RecvError)> {
        self.reader.recv_view(op)
    }

    /// Removes the given reader from the queue subscription lib
    /// Returns true if this is the last reader in a given broadcast unit
    ///
    /// # Examples
    ///
    /// ```
    /// use multiqueue::mpmc_queue;
    /// let (writer, reader) = mpmc_queue(2);
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

pub struct MPMCIter<T> {
    recv: MPMCReceiver<T>,
}

impl<T> Iterator for MPMCIter<T> {
    type Item = T;

    #[inline(always)]
    fn next(&mut self) -> Option<T> {
        match self.recv.recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<T> IntoIterator for MPMCReceiver<T> {
    type Item = T;

    type IntoIter = MPMCIter<T>;

    fn into_iter(self) -> MPMCIter<T> {
        MPMCIter { recv: self }
    }
}

pub struct MPMCRefIter<'a, T: 'a> {
    recv: &'a MPMCReceiver<T>,
}

impl<'a, T> Iterator for MPMCRefIter<'a, T> {
    type Item = T;

    #[inline(always)]
    fn next(&mut self) -> Option<T> {
        match self.recv.recv() {
            Ok(val) => Some(val),
            Err(_) => None,
        }
    }
}

impl<'a, T: 'a> IntoIterator for &'a MPMCReceiver<T> {
    type Item = T;

    type IntoIter = MPMCRefIter<'a, T>;

    fn into_iter(self) -> MPMCRefIter<'a, T> {
        MPMCRefIter { recv: self }
    }
}

pub struct MPMCUniRefIter<'a, R, F: FnMut(&T) -> R, T: 'a> {
    recv: &'a MPMCUniReceiver<T>,
    op: F,
}

impl<'a, R, F: FnMut(&T) -> R, T: 'a> Iterator for MPMCUniRefIter<'a, R, F, T> {
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

pub fn mpmc_queue<T>(capacity: Index) -> (MPMCSender<T>, MPMCReceiver<T>) {
    let (send, recv) = MultiQueue::<MPMC<T>, T>::new(capacity);
    (MPMCSender { sender: send }, MPMCReceiver { reader: recv })
}

pub fn mpmc_queue_with<T, W: Wait + 'static>(capacity: Index,
                                             w: W)
                                             -> (MPMCSender<T>, MPMCReceiver<T>) {
    let (send, recv) = MultiQueue::<MPMC<T>, T>::new_with(capacity, w);
    (MPMCSender { sender: send }, MPMCReceiver { reader: recv })
}

/// Futures variant of mpmc_queue - datastructures implement
/// Sink + Stream at a minor (~30 ns) performance cost to BlockingWait
pub fn mpmc_fut_queue<T: Clone>(capacity: Index) -> (MPMCFutSender<T>, MPMCFutReceiver<T>) {
    let (isend, irecv) = futures_multiqueue::<MPMC<T>, T>(capacity);
    (MPMCFutSender { sender: isend }, MPMCFutReceiver { receiver: irecv })
}

unsafe impl<T: Send> Send for MPMCSender<T> {}
unsafe impl<T: Send> Send for MPMCReceiver<T> {}
unsafe impl<T: Send> Send for MPMCUniReceiver<T> {}



impl<T> MPMCFutSender<T> {
    /// Equivalent to MPMCSender::try_send
    #[inline(always)]
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        self.sender.try_send(val)
    }

    /// Equivalent to MPMCSender::unsubscribe
    pub fn unsubscribe(self) {
        self.sender.unsubscribe()
    }
}

impl<T> MPMCFutReceiver<T> {
    /// Equivalent to MPMCReceiver::try_recv
    #[inline(always)]
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Equivalent to MPMCReceiver::recv
    #[inline(always)]
    pub fn recv(&self) -> Result<T, RecvError> {
        self.receiver.recv()
    }

    /// Identical to MPMCReceiver::unsubscribe
    pub fn unsubscribe(self) -> bool {
        self.receiver.unsubscribe()
    }
}

impl<R, F: FnMut(&T) -> R, T> MPMCFutUniReceiver<R, F, T> {
    /// Equivalent to MPMCReceiver::try_recv using the held operation
    #[inline(always)]
    pub fn try_recv(&mut self) -> Result<R, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Equivalent to MPMCReceiver::recv using the held operation
    #[inline(always)]
    pub fn recv(&mut self) -> Result<R, RecvError> {
        self.receiver.recv()
    }

    /// Identical to MPMCReceiver::unsubscribe
    pub fn unsubscribe(self) -> bool {
        self.receiver.unsubscribe()
    }
}

impl<T> Sink for MPMCFutSender<T> {
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

impl<T> Stream for MPMCFutReceiver<T> {
    type Item = T;
    type Error = ();

    #[inline(always)]
    fn poll(&mut self) -> Poll<Option<T>, ()> {
        self.receiver.poll()
    }
}

impl<R, F: FnMut(&T) -> R, T> Stream for MPMCFutUniReceiver<R, F, T> {
    type Item = R;
    type Error = ();

    #[inline(always)]
    fn poll(&mut self) -> Poll<Option<R>, ()> {
        self.receiver.poll()
    }
}

#[cfg(test)]
mod test {

    use super::mpmc_queue;

    extern crate crossbeam;
    use self::crossbeam::scope;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::sync::mpsc::TryRecvError;
    use std::thread::yield_now;

    #[test]
    fn build_queue() {
        let _ = mpmc_queue::<usize>(10);
    }

    #[test]
    fn push_pop_test() {
        let (writer, reader) = mpmc_queue(1);
        for _ in 0..100 {
            assert!(reader.try_recv().is_err());
            writer.try_send(1 as usize).expect("Push should succeed");
            assert!(writer.try_send(1).is_err());
            assert_eq!(1, reader.try_recv().unwrap());
        }
    }

    fn mpsc(senders: usize, receivers: usize) {
        let (writer, reader) = mpmc_queue(4);
        let sreader = reader.into_single().unwrap();
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
            scope.spawn(move || {
                let mut myv = Vec::new();
                for _ in 0..senders {
                    myv.push(0);
                }
                bref.wait();
                for _ in 0..num_loop * senders {
                    loop {
                        if let Ok(val) = sreader.try_recv_view(|x| *x) {
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
                assert!(sreader.try_recv().is_err());
            });
        });
    }

    #[test]
    fn test_spsc() {
        mpsc(1, 1);
    }

    #[test]
    fn test_mpsc() {
        mpsc(2, 1);
    }

    fn mpmc(senders: usize, receivers: usize) {
        let (writer, reader) = mpmc_queue(10);
        let myb = Barrier::new(receivers + senders);
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
                let this_reader = reader.clone();
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
            reader.unsubscribe();
        });
        assert_eq!(senders * num_loop,
                   counter.load(Ordering::SeqCst));
    }

    #[test]
    fn test_spmc() {
        mpmc(1, 2);
    }

    #[test]
    fn test_mpmc() {
        mpmc(2, 2);
    }

    #[test]
    fn test_baddrop() {
        // This ensures that a bogus arc isn't dropped from the queue
        let (writer, reader) = mpmc_queue(1);
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
            let (writer, reader) = mpmc_queue(1);
            for _ in 0..10 {
                writer.try_send(Dropper::new(&count)).unwrap();
                reader.recv().unwrap();
            }
        }
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_iterator_comp() {
        let (writer, reader) = mpmc_queue::<usize>(10);
        drop(writer);
        for _ in reader {}
    }

    #[test]
    fn test_single_leave_multi() {
        let (writer, reader) = mpmc_queue::<usize>(10);
        let reader2 = reader.clone();
        writer.try_send(1).unwrap();
        writer.try_send(1).unwrap();
        assert_eq!(reader2.recv().unwrap(), 1);
        drop(reader2);
        let reader_s = reader.into_single().unwrap();
        assert!(reader_s.recv_view(|x| *x).is_ok());
    }
}
