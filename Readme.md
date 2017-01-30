# MultiQueue: Fast MPMC Multicast Queue 

MultiQueue is a fast bounded mpmc queue that supports broadcast/multicast style operations [![Build Status](https://travis-ci.org/schets/multiqueue.svg?branch=master)](https://travis-ci.org/schets/multiqueue)

## Overview

Multiqueue is based on the queue design from the LMAX Disruptor, with a few improvements:
  * It can dynamically add/remove producers, and each reader (or sequence in LMAX land) can have multiple consumers
  * It has fast fallbacks for whenever there's a single consumer and/or a single producer and can detect switches at runtime
  * It works on 32 bit systems
  * In most cases, one can view written directly into the queue without copying it
  * It works on rust

One can think of MultiQueue as a sort of [souped up channel/sync_channel](#bench),
with the additional ability to have multiple independent readers each receiving the same stream of data
(this is somewhat confusing since MultiQueue has two different concepts of consumers,
the [Queue Model](#model) section explains that more). So Why would you choose MultiQueue over the built-in channels?
  * MultiQueue does not allocate on push/pop unlike channel, leading to much more predictable latencies
  * Multiqueue is lockless<sup>[1](#ft1)</sup>, 

## <a name = "model">Queue Model</a>

Each logical consumer receieves the event once, but a logical consumer might actually demultiplex the input over a set of consumers       
```
-> @        @
    \      /
     -> -> -> @' (really @+@+@ each compete for a spot)
    /      \
-> @        @
```

If this diagram is redrawn with each of the producers sending in a sequenced element (time goes left  to right):


```
t=1|t=2|    t=3    | t=4| 
1 -> @              @ (1, 2)
      \            /
       -> 2 -> 1 -> -> @' (really @ (1) + @ (2) + @ (nothing yet))
      /            \
2 -> @              @ (1, 2)
```

## <a name = "bench">Benchmarks</a>

### Throughput

More are coming, but here are some basic throughput becnhmarks done on a Intel(R) Xeon(R) CPU E3-1240 v5.
These were done using the busywait method to block, but using a blocking wait on receivers in practice will
be more than fast enough for most use cases


```
@ -> -> @
```
Single Producer Single Consumer: 50-70 million ops per second. In this case, channels do ~8-11 million ops per second


```
         @
        /
@ -> ->   
        \
         @
```
Single Producer Single Consumer, broadcasted to two different readers: 28 million ops per second 


```
         @
        /
@ -> -> -> @
        \
         @
```
Single Producer Single Consumer, broadcasted to three different readers: 25 million ops per second


```
@
 \
  -> -> @
 /
@
```
Multi Producer Single Consumer: 9 million ops per second. In this case, channels do ~8-9 million ops per second.


```
@        @
 \      /
  -> -> 
 /      \
@        @
```
Multi Producer Single Consumer, broadcast to two different readers: 8 million ops per second
### Latency

I need to rewrite the latency benchmark tool,but latencies will be approximately the
inter core communication time, about 40-70 ns. These will be somewhat higher with multiple producers
and multiple consumers of a single subscriber

## Footnotes

<a name = "ft1">1</a>. The queue is technically not lockless - a writer which has claimed a write spot
but then gotten stuck will block readers from progressing. I don't believe there can exist a general
purpose mpmc queue that doesn't use locks and doesn't suffer from that problem.
Operations that involve adding/remove readers and writers themselves block, but I figure that's fine.
 