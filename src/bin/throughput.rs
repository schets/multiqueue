extern crate crossbeam;
extern crate multiqueue;
extern crate time;

use multiqueue::{MulticastReceiver, MulticastSender, multicast_queue, wait};

use time::precise_time_ns;

use crossbeam::scope;

use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};

#[inline(never)]
fn recv(bar: &Barrier, mreader: MulticastReceiver<u64>, sum: &AtomicUsize, check: bool) {
    let reader = mreader.into_single().unwrap();
    bar.wait();
    let start = precise_time_ns();
    let mut cur = 0;
    loop {
        match reader.recv() {
            Ok(pushed) => {
                if cur != pushed {
                    if check {
                        panic!("Got {}, expected {}", pushed, cur);
                    }
                }
                cur += 1;
            }
            Err(_) => break,
        }
    }

    sum.fetch_add((precise_time_ns() - start) as usize, Ordering::SeqCst);
}

fn send(bar: &Barrier, writer: MulticastSender<u64>, num_push: usize) {
    bar.wait();
    for i in 0..num_push as u64 {
        loop {
            let topush = i;
            if let Ok(_) = writer.try_send(topush) {
                break;
            }
        }
    }
}

fn runit(name: &str, n_senders: usize, n_readers: usize) {
    let num_do = 100000000;
    let (writer, reader) = multiqueue_with(20000, wait::BusyWait::new());
    let bar = Barrier::new(1 + n_senders + n_readers);
    let bref = &bar;
    let ns_atomic = AtomicUsize::new(0);
    scope(|scope| {
        for _ in 0..n_senders {
            let w = writer.clone();
            scope.spawn(move || { send(bref, w, num_do); });
        }
        writer.unsubscribe();
        for _ in 0..n_readers {
            let aref = &ns_atomic;
            let r = reader.add_stream();
            let check = n_senders == 1;
            scope.spawn(move || { recv(bref, r, aref, check); });
        }
        reader.unsubscribe();
        bar.wait();
    });
    let ns_spent = (ns_atomic.load(Ordering::Relaxed) as f64) / n_readers as f64;
    let ns_per_item = ns_spent / (num_do as f64);
    println!("Time spent doing {} push/pop pairs for {} was {} ns per item",
             num_do,
             name,
             ns_per_item);
}

fn main() {
    runit("1p::1c", 1, 1);
    runit("1p::1c_2b", 1, 2);
    runit("1p::1c_3b", 1, 3);
    runit("2p::1c", 2, 1);
    runit("2p::1c_2b", 2, 2);
    runit("2p::1c_3b", 2, 3);
}
