
mod alloc;
mod consume;
mod countedindex;
mod maybe_acquire;
mod multiqueue;
mod read_cursor;

pub use multiqueue::{multiqueue, MultiReader, MultiWriter};


#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}
