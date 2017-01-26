extern crate crossbeam;
extern crate multiqueue;
extern crate time;

use multiqueue::{MultiReader, MultiWriter, multiqueue};
use std::sync::mpsc::TryRecvError;

use crossbeam::scope;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::spawn;

const EACH_READER: usize = 100;
const MAX_READERS: usize = 60;
const WRITERS: usize = 20;

#[inline(never)]
fn recv(reader: MultiReader<u64>, how_many_r: Arc<AtomicUsize>) -> u64 {
    let mut cur = 0; 
    for _ in 0..2 {
        for _ in 0..EACH_READER {
            match reader.try_recv() {
                Ok(_) => cur += 1,
                Err(TryRecvError::Disconnected) => return cur,
                _ => (),
            }
        }
        if how_many_r.load(Ordering::SeqCst) < MAX_READERS {
            let new_reader = reader.add_reader();
            how_many_r.fetch_add(1, Ordering::SeqCst);
            let newr = how_many_r.clone();
            spawn(move || { recv(new_reader, newr)});
        }
    }
    let current = how_many_r.load(Ordering::SeqCst);
    if current <= 10 {
        let new_reader = reader.add_reader();
        spawn(move || { recv(new_reader, how_many_r) });
    }
    return cur;
}

fn send(writer: MultiWriter<u64>, num_push: usize) {
    for i in 0..num_push as u64 {
        loop {
            let topush = i;
            if let Ok(_) =  writer.try_send(topush) {
                break;
            }
        }
    }
}

fn main() {
    let num_do = 10000000;
    let mytest = Arc::new(AtomicUsize::new(1));
    let (writer, reader) = multiqueue(200);
    scope(|scope| {
        for _ in 0..WRITERS {
            let cwriter = writer.clone();
            scope.spawn(move || {
                send(cwriter, num_do);
            });
        }
        writer.unsubscribe();
        recv(reader, mytest);
    });
}
