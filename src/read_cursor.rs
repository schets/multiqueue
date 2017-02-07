use std::cell::Cell;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering, fence};

use alloc;
use consume::CONSUME;
use countedindex::{CountedIndex, Index, past, Transaction};
use maybe_acquire::{MAYBE_ACQUIRE, maybe_acquire_fence};
use memory::MemoryManager;

#[derive(Clone, Copy, PartialEq)]
enum ReaderState {
    Single,
    Multi,
}

struct ReaderPos {
    pos_data: CountedIndex,
}

struct ReaderMeta {
    state: Cell<ReaderState>,
    num_consumers: AtomicUsize,
}

#[derive(Clone, Copy)]
pub struct Reader {
    pos: *const ReaderPos,
    meta: *const ReaderMeta,
}

/// This represents the reader attempt at loading a transaction
/// It behaves similarly to a Transaction but has logic for single/multi
/// readers
pub struct ReadAttempt<'a> {
    linked: Transaction<'a>,
    reader: &'a Reader,
    state: ReaderState,
}

/// This holds the set of readers currently active.
/// This struct is held out of line from the cursor so it's easy to atomically replace it
struct ReaderGroup {
    readers: Vec<*const ReaderPos>,
}

#[repr(C)]
pub struct ReadCursor {
    readers: AtomicPtr<ReaderGroup>,
    pub last_pos: Cell<usize>,
}

impl<'a> ReadAttempt<'a> {
    #[inline(always)]
    pub fn get(&self) -> (isize, usize) {
        self.linked.get()
    }

    #[inline(always)]
    pub fn commit_attempt(self, by: Index, ord: Ordering) -> Option<ReadAttempt<'a>> {
        match self.state {
            ReaderState::Single => {
                self.linked.commit_direct(by, ord);
                None
            }
            ReaderState::Multi => {
                if unsafe { (*self.reader.meta).num_consumers.load(Ordering::Relaxed) } == 1 {
                    fence(Ordering::Acquire);
                    unsafe {
                        (*self.reader.meta).state.set(ReaderState::Single);
                    }
                    self.linked.commit_direct(by, ord);
                    None
                } else {
                    match self.linked.commit(by, ord) {
                        Some(transaction) => {
                            Some(ReadAttempt {
                                linked: transaction,
                                reader: self.reader,
                                state: ReaderState::Multi,
                            })
                        }
                        None => None,
                    }
                }
            }
        }
    }

    #[inline(always)]
    pub fn commit_direct(self, by: Index, ord: Ordering) {
        self.linked.commit_direct(by, ord);
    }
}

impl Reader {
    #[inline(always)]
    pub fn load_attempt(&self, ord: Ordering) -> ReadAttempt {
        unsafe {
            ReadAttempt {
                linked: (*self.pos).pos_data.load_transaction(ord),
                reader: self,
                state: (*self.meta).state.get(),
            }
        }
    }

    #[inline(always)]
    pub fn load_count(&self, ord: Ordering) -> usize {
        unsafe { (*self.pos).pos_data.load_count(ord) }
    }

    pub fn dup_consumer(&self) {
        unsafe {
            if (*self.meta).num_consumers.fetch_add(1, Ordering::SeqCst) == 1 {
                (*self.meta).state.set(ReaderState::Multi);
            }
        }
    }

    pub fn remove_consumer(&self) -> usize {
        unsafe { (*self.meta).num_consumers.fetch_sub(1, Ordering::SeqCst) }
    }

    pub fn get_consumers(&self) -> usize {
        unsafe { (*self.meta).num_consumers.load(Ordering::Relaxed) }
    }
}

impl ReaderGroup {
    pub fn new() -> ReaderGroup {
        ReaderGroup { readers: Vec::new() }
    }

    /// Only safe to call from a consumer of the queue!
    pub unsafe fn add_stream(&self, raw: usize, wrap: Index) -> (*mut ReaderGroup, Reader) {
        let new_meta = alloc::allocate(1);
        let new_group = alloc::allocate(1);
        let new_pos = alloc::allocate(1);
        ptr::write(new_pos,
                   ReaderPos { pos_data: CountedIndex::from_usize(raw, wrap) });
        ptr::write(new_meta,
                   ReaderMeta {
                       state: Cell::new(ReaderState::Single),
                       num_consumers: AtomicUsize::new(1),
                   });
        let new_reader = Reader {
            pos: new_pos,
            meta: new_meta as *const ReaderMeta,
        };
        let mut new_readers = self.readers.clone();
        new_readers.push(new_pos as *const ReaderPos);
        ptr::write(new_group, ReaderGroup { readers: new_readers });
        (new_group, new_reader)
    }

    pub unsafe fn remove_reader(&self, reader: *const ReaderPos) -> *mut ReaderGroup {
        let new_group = alloc::allocate(1);
        let mut new_readers = self.readers.clone();
        new_readers.retain(|pt| *pt != reader);
        ptr::write(new_group, ReaderGroup { readers: new_readers });
        new_group
    }

    pub fn get_max_diff(&self, cur_writer: usize) -> Option<Index> {
        let mut max_diff: usize = 0;
        unsafe {
            for reader_ptr in &self.readers {
                // If a reader has passed the writer during this function call
                // then what must have happened is that somebody else has completed this
                // written to the queue, and a reader has bypassed it. We should retry
                let rpos = (**reader_ptr).pos_data.load_count(MAYBE_ACQUIRE);
                let (diff, tofar) = past(cur_writer, rpos);
                if tofar {
                    return None;
                }
                max_diff = if diff > max_diff { diff } else { max_diff };
            }
        }
        maybe_acquire_fence();

        Some(max_diff as Index)
    }
}

impl ReadCursor {
    pub fn new(wrap: Index) -> (ReadCursor, Reader) {
        let rg = ReaderGroup::new();
        unsafe {
            let (real_group, reader) = rg.add_stream(0, wrap);
            (ReadCursor {
                 readers: AtomicPtr::new(real_group),
                 last_pos: Cell::new(0),
             },
             reader)
        }
    }

    #[inline(always)]
    pub fn prefetch_metadata(&self) {
        unsafe {
            let rg = &*self.readers.load(CONSUME);
            let dummy_ptr: *const usize = mem::transmute(&rg.readers);
            ptr::read_volatile(dummy_ptr);
        }
    }

    pub fn get_max_diff(&self, cur_writer: usize) -> Option<Index> {
        loop {
            unsafe {
                let first_ptr = self.readers.load(CONSUME);
                let rg = &*first_ptr;
                let rval = rg.get_max_diff(cur_writer);
                // This check ensures that the pointer hasn't changed
                // We must first read the diff, *and then* check the pointer
                // for changes.
                //
                // We can also get away with having a writer change this during
                // the load as long as it doesn't finish the change since the
                // reader doing the change is pinned to the same spot. It's basically
                // like a seqlock where only the final check is needed and not the first
                // since the data remains valid throughout the write cycle
                //
                // We don't need another acquire fence here since the
                // relevant loads in get_max_diff are going to ordered
                // before all loads after the function exit, and also
                // ordered after the original pointer load
                let second_ptr = self.readers.load(Ordering::Relaxed);
                if second_ptr == first_ptr {
                    return rval;
                }
            }
        }
    }

    pub fn add_stream(&self, reader: &Reader, manager: &MemoryManager) -> Reader {
        let mut current_ptr = self.readers.load(CONSUME);
        loop {
            unsafe {
                let current_group = &*current_ptr;
                let raw = (*reader.pos).pos_data.load_raw(Ordering::Relaxed);
                let wrap = (*reader.pos).pos_data.wrap_at();
                let (new_group, new_reader) = current_group.add_stream(raw, wrap);
                fence(Ordering::SeqCst);
                match self.readers
                    .compare_exchange(current_ptr,
                                      new_group,
                                      Ordering::Relaxed,
                                      Ordering::Relaxed) {
                    Ok(_) => {
                        fence(Ordering::SeqCst);
                        manager.free(current_ptr, 1);
                        return new_reader;
                    }
                    Err(val) => {
                        current_ptr = val;
                        fence(Ordering::Acquire);
                        ptr::read(new_group);
                        alloc::deallocate(new_reader.meta as *mut ReaderMeta, 1);
                        alloc::deallocate(new_reader.pos as *mut ReaderPos, 1);
                        alloc::deallocate(new_group, 1);
                    }
                }
            }
        }
    }

    pub fn remove_reader(&self, reader: &Reader, mem: &MemoryManager) -> bool {
        let mut current_group = self.readers.load(CONSUME);
        loop {
            unsafe {
                let new_group = (*current_group).remove_reader(reader.pos);
                match self.readers
                    .compare_exchange(current_group,
                                      new_group,
                                      Ordering::SeqCst,
                                      Ordering::SeqCst) {
                    Ok(_) => {
                        fence(Ordering::SeqCst);
                        if (*current_group).readers.len() == 1 {
                            self.last_pos.set(reader.load_count(Ordering::Relaxed));
                        }
                        mem.free(current_group, 1);
                        mem.free(reader.pos as *mut ReaderPos, 1);
                        alloc::deallocate(reader.meta as *mut ReaderMeta, 1);
                        return self.has_readers();
                    }
                    Err(val) => {
                        current_group = val;
                        ptr::read(new_group);
                        alloc::deallocate(new_group, 1);
                    }
                }
            }
        }
    }

    pub fn has_readers(&self) -> bool {
        unsafe {
            let current_group = &*self.readers.load(CONSUME);
            current_group.readers.len() == 0
        }
    }
}
