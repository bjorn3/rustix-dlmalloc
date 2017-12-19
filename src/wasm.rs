use core::ptr;

extern {
    #[link_name = "llvm.wasm.current.memory.i32"]
    fn current_memory() -> u32;

    // TODO: this intrinsic actually returns the previous limit, but LLVM
    // doesn't expose that right now. When we upgrade LLVM stop using
    // `current_memory` above. Also handle `-1` as an allocation failure.
    #[link_name = "llvm.wasm.grow.memory.i32"]
    fn grow_memory(pages: u32);
}

pub unsafe fn alloc(size: usize) -> (*mut u8, usize, u32) {
    let pages = size / page_size();
    let cur = current_memory() as usize;
    grow_memory(pages as u32);
    if current_memory() == cur {
        (ptr::null_mut(), 0, 0)
    }
    ((cur * page_size()) as *mut u8, pages * page_size(), 0)
}

pub unsafe fn remap(_ptr: *mut u8, _oldsize: usize, _newsize: usize, _can_move: bool)
    -> *mut u8
{
    // TODO: I think this can be implemented near the end?
    ptr::null_mut()
}

pub unsafe fn free_part(_ptr: *mut u8, _oldsize: usize, _newsize: usize) -> bool {
    false
}

pub unsafe fn free(_ptr: *mut u8, _size: usize) -> bool {
    false
}

pub fn can_release_part(_flags: u32) -> bool {
    false
}

pub fn acquire_global_lock() {
    // single threaded, no need!
}

pub fn release_global_lock() {
    // single threaded, no need!
}

pub fn allocates_zeros() -> bool {
    true
}

pub fn page_size() -> usize {
    64 * 1024
}
