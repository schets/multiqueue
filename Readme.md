Multiqueue is a general purpose lockless multi producer multiconsumer queue with broadcast capabilities.
It has fast fallbacks for whenever there's a single consumer and/or a single producer (detects switches at runtime!) so it can effectively serve for all your queue cases.

Right now, in the spsc case, this queue can do ~100 million transactions per second and the inter-thread latency is only a few ns more than whatever the intercore latency on your hardware is.
You can expect the effective latency to be around 40-70 depending on the hardware and how soon the result is used.

The most general use case of the queue looks something like this:

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


I apologize for anyone who sees the readme in this state, it's certainly not complete.