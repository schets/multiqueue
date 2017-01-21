use std::mem;

pub fn allocate<T>(num: usize) -> *mut T {
    let vec = Vec::<T>::with_capacity(num);
    let rptr = vec.as_ptr();
    mem::forget(vec);
    rptr as *mut T
}

pub fn deallocate<T>(tofree: *mut T, num: usize) {
    unsafe {
        Vec::from_raw_parts(tofree, 0, num);
    }
}
