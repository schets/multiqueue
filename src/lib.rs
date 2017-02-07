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
//! let (send, recv) = multiqueue::mpmc_queue(10);
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
//! let (send, recv) = multiqueue::multicast_queue(4);
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
//! let (send, recv) = multiqueue::multicast_queue(4);
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
//! let (send, recv) = multiqueue::multicast_queue(4);
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
//! // the receiver can be made into a UniReceiver
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
mod mpmc;
mod multiqueue;
mod multicast;
mod read_cursor;
pub mod wait;

pub use multicast::{MulticastSender, MulticastReceiver, MulticastUniReceiver, MulticastFutSender,
                    MulticastFutReceiver, MulticastFutUniReceiver, multicast_queue,
                    multicast_queue_with, multicast_fut_queue};

pub use mpmc::{MPMCSender, MPMCReceiver, MPMCUniReceiver, MPMCFutSender, MPMCFutReceiver,
               MPMCFutUniReceiver, mpmc_queue, mpmc_queue_with, mpmc_fut_queue};
