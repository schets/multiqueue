extern crate crossbeam;
extern crate multiqueue;
extern crate time;

use multiqueue::{MultiReader, MultiWriter, multiqueue};

use time::precise_time_ns;

use crossbeam::scope;

use std::sync::Barrier;

fn recv(bar: &Barrier, reader: MultiReader<Option<u64>>) -> u64 {
    bar.wait();
    let start = precise_time_ns();
    let mut cur = 0; 
    loop {
        if let Ok(popped) = reader.pop() {
            match popped {
                None => break,
                Some(pushed) => {
                    if cur != pushed {
                        panic!("Dang");
                    }
                    cur += 1;
                }
            }
        }
    }

    precise_time_ns() - start
}

fn send(bar: &Barrier, writer: MultiWriter<Option<u64>>, num_push: usize) {
    bar.wait();
    for i in 0..num_push as u64 {
        loop {
            let topush = Some(i);
            if let Ok(_) =  writer.push(topush) {
                break;
            }
        }
    }
    while let Err(_) = writer.push(None) {}
}

fn main() {
    let num_do = 10000000;
    let (writer, reader) = multiqueue(20000);
    let bar = Barrier::new(2);
    let bref = &bar;
    scope(|scope| {
        scope.spawn(move || {
            send(bref, writer, num_do);
        });
        let ns_spent = recv(bref, reader) as f64;
        let ns_per_item = ns_spent / (num_do as f64);
        println!("Time spent doing {} push/pop pairs (without waiting on the popped result!) was {} ns per item", num_do, ns_per_item);
    });
}
