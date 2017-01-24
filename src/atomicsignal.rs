use std::sync::atomic::{AtomicUsize, Ordering};

const UPDATE_EPOCH: usize = 1 << 0;
const NO_READER: usize = 1 << 1;
const NO_WRITER: usize = 1 << 2;
const START_FREE: usize = 1 << 3;

pub struct AtomicSignal {
    flags: AtomicUsize,
}

pub struct LoadedSignal {
    flags: usize,
}

impl AtomicSignal {
    pub fn new() -> AtomicSignal {
        AtomicSignal { flags: AtomicUsize::new(0) }
    }

    pub fn load(&self, ord: Ordering) -> LoadedSignal {
        LoadedSignal { flags: self.flags.load(ord) }
    }

    pub fn set_epoch(&self, ord: Ordering) -> bool {
        let prev = self.flags.fetch_or(UPDATE_EPOCH, ord);
        (prev & UPDATE_EPOCH) != 0
    }

    pub fn clear_epoch(&self, ord: Ordering) -> bool {
        let prev = self.flags.fetch_and(!UPDATE_EPOCH, ord);
        (prev & UPDATE_EPOCH) != 0
    }

    pub fn set_reader(&self, ord: Ordering) -> bool {
        let prev = self.flags.fetch_or(NO_READER, ord);
        (prev & NO_READER) != 0
    }

    pub fn clear_reader(&self, ord: Ordering) -> bool {
        let prev = self.flags.fetch_and(!NO_READER, ord);
        (prev & NO_READER) != 0
    }

    pub fn set_writer(&self, ord: Ordering) -> bool {
        let prev = self.flags.fetch_or(NO_WRITER, ord);
        (prev & NO_WRITER) != 0
    }

    pub fn clear_writer(&self, ord: Ordering) -> bool {
        let prev = self.flags.fetch_and(!NO_READER, ord);
        (prev & NO_READER) != 0
    }

    pub fn set_start_free(&self, ord: Ordering) -> bool {
        let prev = self.flags.fetch_or(START_FREE, ord);
        (prev & START_FREE) != 0
    }

    pub fn clear_start_free(&self, ord: Ordering) -> bool {
        let prev = self.flags.fetch_and(!START_FREE, ord);
        (prev & START_FREE) != 0
    }
}

impl LoadedSignal {
    pub fn get_epoch(&self, ord: Ordering) -> bool {
        (self.flags & UPDATE_EPOCH) != 0
    }

    pub fn get_reader(&self, ord: Ordering) -> bool {
        (self.flags & NO_READER) != 0
    }

    pub fn get_writer(&self, ord: Ordering) -> bool {
        (self.flags & NO_WRITER) != 0
    }

    pub fn start_free(&self, ord: Ordering) -> bool {
        (self.flags & START_FREE) != 0
    }
}
