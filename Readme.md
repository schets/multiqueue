# MultiQueue: Fast MPMC Multicast Queue 

MultiQueue is a fast bounded mpmc queue that supports broadcast/multicast style operations [![Build Status](https://travis-ci.org/schets/multiqueue.svg?branch=master)](https://travis-ci.org/schets/multiqueue)

[Overview](#over)

[Queue Model](#model)

[Examples](#examples)

[Benchmarks](#bench)

[FAQ](#faq)

[Footnotes](#footnotes)

## <a name = "over">Overview</a>


Multiqueue is based on the queue design from the LMAX Disruptor, with a few improvements:
  * It can act as a futures stream/sink, so you can set up high-performance computing pipelines with ease
  * It can dynamically add/remove producers, and each [stream](#model) can have multiple consumers
  * It has fast fallbacks for whenever there's a single consumer and/or a single producer and can detect switches at runtime
  * It works on 32 bit systems without any performance or capability penalty
  * In most cases, one can view data written directly into the queue without copying it

One can think of MultiQueue as a sort of [souped up channel/sync_channel](#bench),
with the additional ability to have multiple independent consumers each receiving the same [stream](#model) of data.
So why would you choose MultiQueue over the built-in channels?
  * MultiQueue support broadcasting elements to multiple readers with a single push into the queue
  * MultiQueue allows reading elements in-place in the queue in most cases, so you can broadcast elements without lots of copying
  * MultiQueue can act as a futures stream and sink
  * MultiQueue does not allocate on push/pop unlike channel, leading to much more predictable latencies
  * Multiqueue is practically lockless<sup>[1](#ft1)</sup> unlike sync_channel, and fares decently under contention

On the other hand, you would want to use a channel/sync_channel if you:
  * Truly want an unbounded queue, although you should probably handle backlog instead
  * Need senders to block when the queue is full and can't use the futures api
  * Don't want the memory usage of a large buffer but need to fit many elements in

Otherwise, in most cases, MultiQueue should be a good replacement for channels.
In general, this will function very well as normal bounded queue with performance
approaching that of hand-written queues for single/multiple consumers/producers
even without taking advantage of the broadcast

## <a name = "model">Queue Model</a>

MultiQueue functions similarly to the LMAX Disruptor from a high level view.
There's an incoming FIFO data stream that is broadcast to a set of subscribers
as if there were multiple streams being written to.
There are two main differences:
  * MultiQueue transparently supports switching between single and multiple producers.
  * Each broadcast stream can be shared among multiple consumers.

The last part makes the model a bit confusing, since there's a difference between a stream of data
and something consuming that stream. To make things worse, each consumer may not actually see each
value on the stream. Instead, multiple consumers may act on a single stream each getting unique access
to certain elements.

A helpful mental model may be to think about this as if each stream was really just an mpmc
queue that was getting pushed to, and the MultiQueue structure just assembled a bunch together behind the scenes.

An diagram tha represents a general use case of the queue where each consumer has unique access to a stream
is below - the # stand in for producers and @ stands in for the consumer of each stream, each with a label.
The lines are meant to show the data flow through the queue.

```
-> #        @-1
    \      /
     -> -> -> @-2
    /      \
-> #        @-3
```
This is a pretty standard broadcast queue setup - 
for each element sent in, it is seen on each stream by that's streams consumer.
 

However, in MultiQueue, each logical consumer might actually be demultiplexed across many actual consumers, like below.
```
-> #        @-1
    \      /
     -> -> -> @-2' (really @+@+@ each compete for a spot)
    /      \
-> #        @-3
```

If this diagram is redrawn with each of the producers sending in a sequenced element (time goes left  to right):


```
t=1|t=2|    t=3    | t=4| 
1 -> #              @-1 (1, 2)
      \            /
       -> 2 -> 1 -> -> @-2' (really @ (1) + @ (2) + @ (nothing yet))
      /            \
2 -> #              @-3 (1, 2)
```

If one imagines this as a webserver, the streams for @-1 and @-3 might be doing random webservery work like some logging
or metrics gathering and can handle the workload completely on one core, @-2 is doing expensive work handling requests
and is split into multiple workers dealing with the data stream.

Since that probably made no sense, here are some examples

## <a name = "examples">Examples</a>

## Single-producer single-stream

This is about as simple as it gets for a queue. Fast, one writer, one reader, simple to use.
```rust
extern crate multiqueue;

use std::thread;

let (send, recv) = multiqueue::new(10);

thread::spawn(move || {
    for val in recv {
        println!("Got {}", val);
    }
});

for i in 0..10 {
    send.try_send(i).unwrap();
}

// Drop the sender to close the queue
drop(send);

// prints
// Got 0
// Got 1
// Got 2
// etc


// some join mechanics here
```

## Single-producer double stream.

Let's send the values to two different streams
```rust
extern crate multiqueue;

use std::thread;

let (send, recv) = multiqueue::new(4);

for i in 0..2 { // or n
    let cur_recv = recv.add_stream();
    thread::spawn(move || {
        for val in cur_recv {
            println!("Stream {} got {}", i, val);
        }
    });
}

// Take notice that I drop the reader - this removes it from
// the queue, meaning that the readers in the new threads
// won't get starved by the lack of progress from recv
recv.unsubscribe();

for i in 0..10 {
    // Don't do this busy loop in real stuff unless you're really sure
    loop {
        if send.try_send(i).is_ok() {
            break;
        }
    }
}

// Drop the sender to close the queue
drop(send);

// prints along the lines of
// Stream 0 got 0
// Stream 0 got 1
// Stream 1 got 0
// Stream 0 got 2
// Stream 1 got 1
// etc

// some join mechanics here
```

## Single-producer double stream, 2 consumers per stream
Let's take the above and make each stream consumed by two consumers
```rust
extern crate multiqueue;

use std::thread;

let (send, recv) = multiqueue::new(4);

for i in 0..2 { // or n
    let cur_recv = recv.add_stream();
    for j in 0..2 {
        let stream_consumer = cur_recv.clone();
        thread::spawn(move || {
            for val in stream_consumer {
                println!("Stream {} consumer {} got {}", i, j, val);
            }
        });
    }
    // cur_recv is dropped here
}

// Take notice that I drop the reader - this removes it from
// the queue, meaning that the readers in the new threads
// won't get starved by the lack of progress from recv
recv.unsubscribe();

for i in 0..10 {
    // Don't do this busy loop in real stuff unless you're really sure
    loop {
        if send.try_send(i).is_ok() {
            break;
        }
    }
}
drop(send);

// prints along the lines of
// Stream 0 consumer 1 got 2
// Stream 0 consumer 0 got 0
// Stream 1 consumer 0 got 0
// Stream 0 consumer 1 got 1
// Stream 1 consumer 1 got 1
// Stream 1 consumer 0 got 2
// etc

// some join mechanics here
```

## Something wack`y
Has anyone really been far even as decided to use even go want to do look more like?

```rust
extern crate multiqueue;

use std::thread;

let (send, recv) = multiqueue::new(4);

// start like before
for i in 0..2 { // or n
    let cur_recv = recv.add_stream();
    for j in 0..2 {
        let stream_consumer = cur_recv.clone();
        thread::spawn(move || {
            for val in stream_consumer {
                println!("Stream {} consumer {} got {}", i, j, val);
            }
        });
    }
    // cur_recv is dropped here
}

// On this stream, since there's only one consumer,
// the receiver can be made into a SingleReceiver
// which can view items inline in the queue
let single_recv = recv.add_stream().into_single().unwrap();

thread::spawn(move || {
    for val in single_recv.iter_with(|item_ref| 10 * *item_ref) {
        println!("{}", val);
    }
});

// Same as above, except this time we just want to iterate until the receiver is empty
let single_recv_2 = recv.add_stream().into_single().unwrap();

thread::spawn(move || {
    for val in single_recv_2.partial_iter_with(|item_ref| 10 * *item_ref) {
        println!("{}", val);
    }
});

// Take notice that I drop the reader - this removes it from
// the queue, meaning that the readers in the new threads
won't get starved by the lack of progress from recv
recv.unsubscribe();

// Many senders to give all the receivers something
for _ in 0..3 {
    let cur_send = send.clone();
    for i in 0..10 {
        // Don't do this busy loop in real stuff unless you're really sure
        loop {
            if cur_send.try_send(i).is_ok() {
                break;
            }
        }
    }
}
drop(send);

// prints along the lines of
// Stream 0 consumer 1 got 0
// Stream 0 consumer 0 got 1
// Stream 1 consumer 0 got 0
// Stream 0 consumer 1 got 2
// Stream 1 consumer 1 got 1
// Stream 1 consumer 0 got 2
// etc

// some join mechanics here
```

## <a name = "bench">Benchmarks</a>

### Throughput

More are coming, but here are some basic throughput becnhmarks done on a Intel(R) Xeon(R) CPU E3-1240 v5.
These were done using the busywait method to block, but using a blocking wait on receivers in practice will
be more than fast enough for most use cases.


Single Producer Single Consumer: 50-70 million ops per second. In this case, channels do ~8-11 million ops per second
```
# -> -> @
```
____
Single Producer Single Consumer, broadcasted to two different streams: 28 million ops per second<sup>[2](#ft2)</sup>
```
         @
        /
# -> ->   
        \
         @
```
____
Single Producer Single Consumer, broadcasted to three different streams: 25 million ops per second<sup>[2](#ft2)</sup>
```
         @
        /
# -> -> -> @
        \
         @
```
____
Multi Producer Single Consumer: 9 million ops per second. In this case, channels do ~8-9 million ops per second.
```
#
 \
  -> -> @
 /
#
```
____
Multi Producer Single Consumer, broadcast to two different streams: 8 million ops per second
```
#        @
 \      /
  -> -> 
 /      \
#        @
```
### Latency

I need to rewrite the latency benchmark tool, but latencies will be approximately the
inter core communication delay, about 40-70 ns on a single socket machine.
These will be somewhat higher with multiple producers
and multiple consumers since each one must perform an RMW before finishing a write or read.

## <a name = "faq">FAQ</a>

#### Why can't senders block even though readers can?
It's sensible for a reader to block if there is truly nothing for it to do, while the equivalent
isn't true for senders. If a sender blocks, that means that the system is backlogged and
something should be done about it (like starting another worker that consumes from the backed up stream)!
Furthermore, it puts more of a performance penalty on the queue than I really wanted and the latency
hit for notifying senders comes before the queue action is finished, while notifying readers happens
after the value has sent.

#### Why can the futures sender park even though senders can't block?
It's required for futures api to work sensibly, since when futures can't send into the queue
it expects that the task will be parked and awoken by some other process (if this is wrong, please let me know!).
That makes sense as well since other events will be handled during that time instead of plain blocking.
I'm probably going to add a futures api that just spins on the queue for people who want the niceness of
the futures api but don't want the performance hit.

#### I want to know which stream is the farthest behind when there's backlog, can I do that?
As of now, that's not possible to do. In general, that sort of question is difficult to concretely
answer because any attempt to answer it will be racing against writer updates, and there's also no way
to transform the idea of 'which stream is behind' into something actionable by a program.

#### Is it possible to select from a set of MultiQueues?
No, it is not currently. Since performance is a key feature of the queue, I wouldn't want
to make amends to allow select if they would negatively affect performance.

#### What happens if consumers of one stream fall behind?
The queue won't overwrite a datapoint until all streams have advanced past it,
so writes to the queue would fail. Depending on your goals, this is either a good or a bad thing.
On one hand, nobody likes getting blocked/starved of updates because of some dumb slow thread.
On the other hand, this basically enforces a sort of system-wide backlog control. If you want
an example why that's needed, NYSE occasionally does not keep the consolidated feed
up to date with the individual feeds and markets fall into disarray.

## <a name = "footnotes">Footnotes</a>

<a name = "ft1">1</a>. The queue is technically not lockless - a writer which has claimed a write spot
but then gotten stuck will block readers from progressing. I don't believe there can exist a general purpose
mpmc ringbuffer based queue which doesn't suffer from that problem, and in practice, it will rarely ever matter.
Operations that involve adding/remove readers and writers themselves block, but I figure that's fine.
 
<a name = "ft2">2</a> These benchmarks had extremely varying benchmarks so I took the upper bound. On some other machines
they showed only minor performance differences compared to the spsc case so
I think in practice the effective throughput will be much higher