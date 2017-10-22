#![feature(allocator_api, alloc)]
#![no_std]

extern crate alloc;

use alloc::heap::{Alloc, Layout, Excess, CannotReallocInPlace, AllocErr};
use core::mem;
use core::ptr;

#[cfg(unix)]
#[path = "unix.rs"]
mod sys;

pub struct Dlmalloc {
    smallmap: u32,
    treemap: u32,
    smallbins: [*mut Chunk; (NSMALLBINS + 1) * 2],
    treebins: [*mut TreeChunk; NTREEBINS],
    dvsize: usize,
    topsize: usize,
    dv: *mut Chunk,
    top: *mut Chunk,
    footprint: usize,
    max_footprint: usize,
    seg: Segment,
    trim_check: usize,
    least_addr: *mut u8,
    release_checks: usize,
}

const NSMALLBINS: usize = 32;
const NTREEBINS: usize = 32;
const SMALLBIN_SHIFT: usize = 3;
const TREEBIN_SHIFT: usize = 8;
const DEFAULT_GRANULARITY: usize = 64 * 1024;
const DEFAULT_TRIM_THRESHOLD: usize = 2 * 1024 * 1024;
const MAX_RELEASE_CHECK_RATE: usize = 4095;

#[repr(C)]
struct Chunk {
    prev_foot: usize,
    head: usize,
    prev: *mut Chunk,
    next: *mut Chunk,
}

#[repr(C)]
struct TreeChunk {
    chunk: Chunk,
    child: [*mut TreeChunk; 2],
    parent: *mut TreeChunk,
    index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Segment {
    base: *mut u8,
    size: usize,
    next: *mut Segment,
    flags: u32,
}

fn align_up(a: usize, alignment: usize) -> usize {
    assert!(alignment.is_power_of_two());
    (a + (alignment - 1)) & !(alignment - 1)
}

fn left_bits(x: u32) -> u32 {
    (x << 1) | (!(x << 1) + 1)
}

fn least_bit(x: u32) -> u32 {
    x & (!x + 1)
}

fn leftshift_for_tree_index(x: u32) -> u32 {
    let x = x as usize;
    if x == NTREEBINS - 1 {
        0
    } else {
        (mem::size_of::<usize>() * 8 - 1 - ((x >> 1) + TREEBIN_SHIFT - 2)) as u32
    }
}

impl Dlmalloc {
    pub fn new() -> Dlmalloc {
        Dlmalloc {
            smallmap: 0,
            treemap: 0,
            smallbins: [ptr::null_mut(); (NSMALLBINS + 1) * 2],
            treebins: [ptr::null_mut(); NTREEBINS],
            dvsize: 0,
            topsize: 0,
            dv: ptr::null_mut(),
            top: ptr::null_mut(),
            footprint: 0,
            max_footprint: 0,
            seg: Segment {
                base: ptr::null_mut(),
                size: 0,
                next: ptr::null_mut(),
                flags: 0,
            },
            trim_check: 0,
            least_addr: ptr::null_mut(),
            release_checks: 0,
        }
    }

    // TODO: can we get rid of this?
    fn malloc_alignment(&self) -> usize {
        mem::size_of::<usize>() * 2
    }

    // TODO: dox
    fn chunk_overhead(&self) -> usize {
        mem::size_of::<usize>()
    }

    // TODO: dox
    fn min_large_size(&self) -> usize {
        1 << TREEBIN_SHIFT
    }

    // TODO: dox
    fn max_small_size(&self) -> usize {
        self.min_large_size() - 1
    }

    // TODO: dox
    fn max_small_request(&self) -> usize {
        self.max_small_size() - (self.malloc_alignment() - 1) - self.chunk_overhead()
    }

    // TODO: dox
    fn min_chunk_size(&self) -> usize {
        align_up(mem::size_of::<Chunk>(), self.malloc_alignment())
    }

    // TODO: dox
    fn min_request(&self) -> usize {
        self.min_chunk_size() - self.chunk_overhead() - 1
    }

    // TODO: dox
    fn max_request(&self) -> usize {
        (!self.min_chunk_size() + 1) << 2
    }

    fn pad_request(&self, amt: usize) -> usize {
        align_up(amt + self.chunk_overhead(), self.malloc_alignment())
    }

    fn small_index(&self, size: usize) -> u32 {
        (size >> SMALLBIN_SHIFT) as u32
    }

    fn small_index2size(&self, idx: u32) -> usize {
        (idx as usize) << SMALLBIN_SHIFT
    }

    fn is_small(&self, s: usize) -> bool {
        s >> SMALLBIN_SHIFT < NSMALLBINS
    }

    fn is_aligned(&self, a: usize) -> bool {
        a & (self.malloc_alignment() - 1) == 0
    }

    fn align_offset(&self, addr: *mut u8) -> usize {
        align_up(addr as usize, self.malloc_alignment()) - (addr as usize)
    }

    fn top_foot_size(&self) -> usize {
        self.align_offset(unsafe { Chunk::to_mem(ptr::null_mut()) }) +
            self.pad_request(mem::size_of::<Segment>()) +
            self.min_chunk_size()
    }

    fn align_as_chunk(&self, ptr: *mut u8) -> *mut Chunk {
        unsafe {
            let chunk = Chunk::to_mem(ptr as *mut Chunk);
            ptr.offset(self.align_offset(chunk) as isize)
                as *mut Chunk
        }
    }

    unsafe fn malloc(&mut self, size: usize) -> *mut u8 {
        self.check_malloc_state();

        let nb;
        if size <= self.max_small_request() {
            nb = if size < self.min_request() {
                self.min_chunk_size()
            } else {
                self.pad_request(size)
            };
            let mut idx = self.small_index(nb);
            let smallbits = self.smallmap >> idx;

            // Check the bin for `idx` (the lowest bit) but also check the next
            // bin up to use that to satisfy our request, if needed.
            if smallbits & 0b11 != 0 {
                // If our the lowest bit, our `idx`, is unset then bump up the
                // index as we'll be using the next bucket up.
                idx += !smallbits & 1;

                let b = self.smallbin_at(idx);
                let p = (*b).prev;
                self.unlink_first_small_chunk(b, p, idx);
                let smallsize = self.small_index2size(idx);
                Chunk::set_inuse_and_pinuse(p, smallsize);
                let ret = Chunk::to_mem(p);
                self.check_malloced_chunk(ret, nb);
                return ret
            }

            if nb > self.dvsize {
                // If there's some other bin with some memory, then we just use
                // the next smallest bin
                if smallbits != 0 {
                    let leftbits = (smallbits << idx) & left_bits(1 << idx);
                    let leastbit = least_bit(leftbits);
                    let i = leastbit.trailing_zeros();
                    let b = self.smallbin_at(i);
                    let p = (*b).prev;
                    assert_eq!(Chunk::size(p), self.small_index2size(i));
                    self.unlink_first_small_chunk(b, p, i);
                    let smallsize = self.small_index2size(i);
                    let rsize = smallsize - nb;
                    if mem::size_of::<usize>() != 4 && rsize < self.min_chunk_size() {
                        Chunk::set_inuse_and_pinuse(p, smallsize);
                    } else {
                        Chunk::set_size_and_pinuse_of_inuse_chunk(p, nb);
                        let r = Chunk::plus_offset(p, nb);
                        Chunk::set_size_and_pinuse_of_free_chunk(r, rsize);
                        self.replace_dv(r, rsize);
                    }
                    let ret = Chunk::to_mem(p);
                    self.check_malloced_chunk(ret, nb);
                    return ret

                } else if self.treemap != 0 {
                    let mem = self.tmalloc_small(nb);
                    if !mem.is_null() {
                        self.check_malloced_chunk(mem, nb);
                        self.check_malloc_state();
                        return mem
                    }
                }
            }
        } else if size >= self.max_request() {
            // TODO: translate this to unsupported
            return ptr::null_mut()
        } else {
            nb = self.pad_request(size);
            if self.treemap != 0 {
                let mem = self.tmalloc_large(nb);
                if !mem.is_null() {
                    self.check_malloced_chunk(mem, nb);
                    self.check_malloc_state();
                    return mem
                }
            }
        }

        // use the `dv` node if we can, splitting it if necessary or otherwise
        // exhausting the entire chunk
        if nb <= self.dvsize {
            let rsize = self.dvsize - nb;
            let p = self.dv;
            if rsize >= self.min_chunk_size() {
                self.dv = Chunk::plus_offset(p, nb);
                self.dvsize = rsize;
                let r = self.dv;
                Chunk::set_size_and_pinuse_of_free_chunk(r, rsize);
                Chunk::set_size_and_pinuse_of_inuse_chunk(p, nb);
            } else {
                let dvs = self.dvsize;
                self.dvsize = 0;
                self.dv = ptr::null_mut();
                Chunk::set_inuse_and_pinuse(p, dvs);
            }
            let ret = Chunk::to_mem(p);
            self.check_malloced_chunk(ret, nb);
            self.check_malloc_state();
            return ret
        }

        // Split the top node if we can
        if nb < self.topsize {
            self.topsize -= nb;
            let rsize = self.topsize;
            let p = self.top;
            self.top = Chunk::plus_offset(p, nb);
            let r = self.top;
            (*r).head = rsize | PINUSE;
            Chunk::set_size_and_pinuse_of_inuse_chunk(p, nb);
            self.check_top_chunk(self.top);
            let ret = Chunk::to_mem(p);
            self.check_malloced_chunk(ret, nb);
            self.check_malloc_state();
            return ret
        }

        self.sys_alloc(nb)
    }

    unsafe fn sys_alloc(&mut self, size: usize) -> *mut u8 {
        self.check_malloc_state();
        let asize = align_up(size + self.top_foot_size() + self.malloc_alignment(),
                             DEFAULT_GRANULARITY);

        let (tbase, tsize, flags) = sys::alloc(asize);
        if tbase.is_null() {
            return tbase
        }

        self.footprint += tsize;
        if self.footprint > self.max_footprint {
            self.max_footprint = self.footprint;
        }

        if self.top.is_null() {
            if self.least_addr.is_null() || tbase < self.least_addr {
                self.least_addr = tbase;
            }
            self.seg.base = tbase;
            self.seg.size = tsize;
            self.seg.flags = flags;
            self.release_checks = MAX_RELEASE_CHECK_RATE;
            self.init_bins();
            let tsize = tsize - self.top_foot_size();
            self.init_top(tbase as *mut Chunk, tsize);
            // let mn = Chunk::next(Chunk::from_mem(self as *mut _ as *mut u8));
            // let top_foot_size = self.top_foot_size();
            // self.init_top(mn, tbase as usize + tsize - mn as usize - top_foot_size);
        } else {
            let mut sp = &mut self.seg as *mut Segment;
            while !sp.is_null() && tbase != Segment::top(sp) {
                sp = (*sp).next;
            }
            if !sp.is_null() &&
                !Segment::is_extern(sp) &&
                Segment::sys_flags(sp) == flags &&
                Segment::holds(sp, self.top as *mut u8)
            {
                (*sp).size += tsize;
                let ptr = self.top;
                let size = self.topsize + tsize;
                self.init_top(ptr, size);
            } else {
                if tbase < self.least_addr {
                    self.least_addr = tbase;
                }
                let mut sp = &mut self.seg as *mut Segment;
                while !sp.is_null() && (*sp).base != tbase.offset(tsize as isize) {
                    sp = (*sp).next;
                }
                if !sp.is_null() &&
                    !Segment::is_extern(sp) &&
                    Segment::sys_flags(sp) == flags
                {
                    let oldbase = (*sp).base;
                    (*sp).base = tbase;
                    (*sp).size += tsize;
                    return self.prepend_alloc(tbase, oldbase, size);
                } else {
                    self.add_segment(tbase, tsize, flags);
                }
            }
        }

        if size < self.topsize {
            self.topsize -= size;
            let rsize = self.topsize;
            let p = self.top;
            self.top = Chunk::plus_offset(p, size);
            let r = self.top;
            (*r).head = rsize | PINUSE;
            Chunk::set_size_and_pinuse_of_inuse_chunk(p, size);
            let ret = Chunk::to_mem(p);
            self.check_top_chunk(self.top);
            self.check_malloced_chunk(ret, size);
            self.check_malloc_state();
            return ret
        }

        return ptr::null_mut()
    }

    unsafe fn init_top(&mut self, ptr: *mut Chunk, size: usize) {
        let offset = self.align_offset(Chunk::to_mem(ptr));
        let p = Chunk::plus_offset(ptr, offset);
        let size = size - offset;

        self.top = p;
        self.topsize = size;
        (*p).head = size | PINUSE;
        (*Chunk::plus_offset(p, size)).head = self.top_foot_size();
        self.trim_check = DEFAULT_TRIM_THRESHOLD;
    }

    unsafe fn init_bins(&mut self) {
        for i in 0..NSMALLBINS as u32 {
            let bin = self.smallbin_at(i);
            (*bin).next = bin;
            (*bin).prev = bin;
        }
    }

    unsafe fn prepend_alloc(&mut self,
                            newbase: *mut u8,
                            oldbase: *mut u8,
                            size: usize) -> *mut u8 {
        let p = self.align_as_chunk(newbase);
        let mut oldfirst = self.align_as_chunk(oldbase);
        let psize = oldfirst as usize - p as usize;
        let q = Chunk::plus_offset(p, size);
        let mut qsize = psize - size;
        Chunk::set_size_and_pinuse_of_inuse_chunk(p, size);

        assert!(oldfirst > q);
        assert!(Chunk::pinuse(oldfirst));
        assert!(qsize >= self.min_chunk_size());


        // consolidate the remainder with the first chunk of the old base
        if oldfirst == self.top {
            self.topsize += qsize;
            let tsize = self.topsize ;
            self.top = q;
            (*q).head = tsize | PINUSE;
            self.check_top_chunk(q);
        } else if oldfirst == self.dv {
            self.dvsize += qsize;
            let dsize = self.dvsize;
            self.dv = q;
            Chunk::set_size_and_pinuse_of_free_chunk(q, dsize);
        } else {
            if !Chunk::inuse(oldfirst) {
                let nsize = Chunk::size(oldfirst);
                self.unlink_chunk(oldfirst, nsize);
                oldfirst = Chunk::plus_offset(oldfirst, nsize);
                qsize += nsize;
            }
            Chunk::set_free_with_pinuse(q, qsize, oldfirst);
            self.insert_chunk(q, qsize);
            self.check_free_chunk(q);
        }

        let ret = Chunk::to_mem(p);
        self.check_malloced_chunk(ret, size);
        self.check_malloc_state();
        return ret
    }

    // add a segment to hold a new noncontiguous region
    unsafe fn add_segment(&mut self, tbase: *mut u8, tsize: usize, flags: u32) {
        // TODO: what in the world is this function doing

        // Determine locations and sizes of segment, fenceposts, and the old top
        let old_top = self.top as *mut u8;
        let oldsp = self.segment_holding(old_top);
        let old_end = Segment::top(oldsp);
        let ssize = self.pad_request(mem::size_of::<Segment>());
        let offset = ssize + mem::size_of::<usize>() * 4 + self.malloc_alignment() - 1;
        let rawsp = old_end.offset(-(offset as isize));
        let offset = self.align_offset(Chunk::to_mem(rawsp as *mut Chunk));
        let asp = rawsp.offset(offset as isize);
        let csp = if asp < old_top.offset(self.min_chunk_size() as isize) {
            old_top
        } else {
            asp
        };
        let sp = csp as *mut Chunk;
        let ss = Chunk::to_mem(sp) as *mut Segment;
        let tnext = Chunk::plus_offset(sp, ssize);
        let mut p = tnext;
        let mut nfences = 0;

        // reset the top to our new space
        let size = tsize - self.top_foot_size();
        self.init_top(tbase as *mut Chunk, size);

        // set up our segment record
        assert!(self.is_aligned(ss as usize));
        Chunk::set_size_and_pinuse_of_inuse_chunk(sp, ssize);
        *ss = self.seg; // push our current record
        self.seg.base = tbase;
        self.seg.size = tsize;
        self.seg.flags = flags;
        self.seg.next = ss;

        // insert trailing fences
        loop {
            let nextp = Chunk::plus_offset(p, mem::size_of::<usize>());
            (*p).head = Chunk::fencepost_head();
            nfences += 1;
            if (&(*nextp).head as *const usize as *mut u8) < old_end {
                p = nextp;
            } else {
                break
            }
        }
        assert!(nfences >= 2);

        // insert the rest of the old top into a bin as an ordinary free chunk
        if csp != old_top {
            let q = old_top as *mut Chunk;
            let psize = csp as usize - old_top as usize;
            let tn = Chunk::plus_offset(q, psize);
            Chunk::set_free_with_pinuse(q, psize, tn);
            self.insert_chunk(q, psize);
        }

        self.check_top_chunk(self.top);
        self.check_malloc_state();
    }

    unsafe fn segment_holding(&self, ptr: *mut u8) -> *mut Segment {
        let mut sp = &self.seg as *const Segment as *mut Segment;
        while !sp.is_null() {
            if (*sp).base <= ptr && ptr < Segment::top(sp) {
                return sp
            }
            sp = (*sp).next;
        }
        ptr::null_mut()
    }

    unsafe fn tmalloc_small(&mut self, size: usize) -> *mut u8 {
        let leastbit = least_bit(self.treemap);
        let i = leastbit.trailing_zeros();
        let mut v = *self.treebin_at(i);
        let mut t = v;
        let mut rsize = Chunk::size(TreeChunk::chunk(t)) - size;

        loop {
            t = TreeChunk::leftmost_child(t);
            if t.is_null() {
                break
            }
            let trem = Chunk::size(TreeChunk::chunk(t)) - size;
            if trem < rsize {
                rsize = trem;
                v = t;
            }
        }

        let vc = TreeChunk::chunk(v);
        let r = Chunk::plus_offset(vc, size) as *mut TreeChunk;
        assert_eq!(Chunk::size(vc), rsize + size);
        self.unlink_large_chunk(v);
        if rsize < self.min_chunk_size() {
            Chunk::set_inuse_and_pinuse(vc, rsize + size);
        } else {
            let rc = TreeChunk::chunk(r);
            Chunk::set_size_and_pinuse_of_inuse_chunk(vc, size);
            Chunk::set_size_and_pinuse_of_free_chunk(rc, rsize);
            self.replace_dv(rc, rsize);
        }
        Chunk::to_mem(vc)
    }

    unsafe fn tmalloc_large(&mut self, size: usize) -> *mut u8 {
        let mut v = ptr::null_mut();
        let mut rsize = !size + 1;
        let idx = self.compute_tree_index(size);
        let mut t = *self.treebin_at(idx);
        if !t.is_null() {
            // Traverse thre tree for this bin looking for a node with size
            // equal to the `size` above.
            let mut sizebits = size << leftshift_for_tree_index(idx);
            // Keep track of the deepest untaken right subtree
            let mut rst = ptr::null_mut();
            loop {
                let csize = Chunk::size(TreeChunk::chunk(t));
                if csize >= size && csize - size < rsize {
                    v = t;
                    rsize = csize - size;
                    if rsize == 0  {
                        break
                    }
                }
                let rt = (*t).child[1];
                t = (*t).child[(sizebits >> (mem::size_of::<usize>() * 8 - 1)) & 1];
                if !rt.is_null() && rt != t {
                    rst = rt;
                }
                if t.is_null() {
                    // Reset `t` to the least subtree holding sizes greater than
                    // the `size` above, breaking out
                    t = rst;
                    break
                }
                sizebits <<= 1;
            }
        }

        // Set t to the root of the next non-empty treebin
        if t.is_null() && v.is_null() {
            let leftbits = left_bits(1 << idx) & self.treemap;
            if leftbits != 0 {
                let leastbit = least_bit(leftbits);
                let i = leastbit.trailing_zeros();
                t = *self.treebin_at(i);
            }
        }

        // Find the smallest of this tree or subtree
        while !t.is_null() {
            let trem = Chunk::size(TreeChunk::chunk(t)) - size;
            if trem < rsize {
                rsize = trem;
                v = t;
            }
            t = TreeChunk::leftmost_child(t);
        }

        // If dv is a better fit, then return null so malloc will use it
        if v.is_null() || rsize + size >= self.dvsize {
            return ptr::null_mut()
        }

        let vc = TreeChunk::chunk(v);
        let r = Chunk::plus_offset(vc, size);
        assert_eq!(Chunk::size(vc), rsize + size);
        self.unlink_large_chunk(v);
        if rsize < self.min_chunk_size() {
            Chunk::set_inuse_and_pinuse(vc, rsize + size);
        } else {
            Chunk::set_size_and_pinuse_of_inuse_chunk(vc, size);
            Chunk::set_size_and_pinuse_of_free_chunk(r, rsize);
            self.insert_chunk(r, rsize);
        }
        Chunk::to_mem(vc)
    }

    unsafe fn smallbin_at(&mut self, idx: u32) -> *mut Chunk {
        &mut self.smallbins[(idx as usize) * 2] as *mut *mut Chunk as *mut Chunk
    }

    unsafe fn treebin_at(&mut self, idx: u32) -> *mut *mut TreeChunk {
        &mut self.treebins[idx as usize]
    }

    fn compute_tree_index(&self, size: usize) -> u32 {
        let x = size >> TREEBIN_SHIFT;
        if x == 0 {
            0
        } else if x > 0xffff {
            NTREEBINS as u32 - 1
        } else {
            let k = mem::size_of_val(&x) * 8 - 1 - (x.leading_zeros() as usize);
            ((k << 1) + (size >> (k + TREEBIN_SHIFT - 1) & 1)) as u32
        }
    }

    unsafe fn unlink_first_small_chunk(&mut self,
                                       head: *mut Chunk,
                                       next: *mut Chunk,
                                       idx: u32) {
        let ptr = (*next).prev;
        assert!(next != head);
        assert!(next != ptr);
        assert_eq!(Chunk::size(next), self.small_index2size(idx));
        if head == ptr {
            self.clear_smallmap(idx);
        } else {
            (*ptr).next = head;
            (*head).prev = ptr;
        }
    }

    unsafe fn replace_dv(&mut self, chunk: *mut Chunk, size: usize) {
        let dvs = self.dvsize;
        assert!(self.is_small(dvs));
        if dvs != 0 {
            let dv = self.dv;
            self.insert_small_chunk(dv, dvs);
        }
        self.dvsize = size;
        self.dv = chunk;
    }

    unsafe fn insert_chunk(&mut self, chunk: *mut Chunk, size: usize) {
        if self.is_small(size) {
            self.insert_small_chunk(chunk, size);
        } else {
            self.insert_large_chunk(chunk as *mut TreeChunk, size);
        }
    }

    unsafe fn insert_small_chunk(&mut self, chunk: *mut Chunk, size: usize) {
        let idx = self.small_index(size);
        let head = self.smallbin_at(idx);
        let mut f = head;
        assert!(size >= self.min_chunk_size());
        if !self.smallmap_is_marked(idx) {
            self.mark_smallmap(idx);
        } else {
            f = (*head).prev;
        }

        (*head).prev = chunk;
        (*f).next = chunk;
        (*chunk).prev = f;
        (*chunk).next = head;
    }

    unsafe fn insert_large_chunk(&mut self, chunk: *mut TreeChunk, size: usize) {
        let idx = self.compute_tree_index(size);
        let h = self.treebin_at(idx);
        (*chunk).index = idx;
        (*chunk).child[0] = ptr::null_mut();
        (*chunk).child[1] = ptr::null_mut();
        let chunkc = TreeChunk::chunk(chunk);
        if !self.treemap_is_marked(idx) {
            self.mark_treemap(idx);
            *h = chunk;
            (*chunk).parent = h as *mut TreeChunk; // TODO: dubious?
            (*chunkc).next = chunkc;
            (*chunkc).prev = chunkc;
        } else {
            let mut t = *h;
            let mut k = size << leftshift_for_tree_index(idx);
            loop {
                if Chunk::size(TreeChunk::chunk(t)) != size {
                    let c = &mut (*t).child[(k >> mem::size_of::<usize>() * 8 - 1) & 1];
                    k <<= 1;
                    if !c.is_null() {
                        t = *c;
                    } else {
                        *c = chunk;
                        (*chunk).parent = t;
                        (*chunkc).next = chunkc;
                        (*chunkc).prev = chunkc;
                        break
                    }
                } else {
                    let tc = TreeChunk::chunk(t);
                    let f = (*tc).prev;
                    (*f).next = chunkc;
                    (*tc).prev = chunkc;
                    (*chunkc).prev = f;
                    (*chunkc).next = tc;
                    (*chunk).parent = ptr::null_mut();
                    break
                }
            }
        }
    }

    unsafe fn smallmap_is_marked(&self, idx: u32) -> bool {
        self.smallmap & (1 << idx) != 0
    }

    unsafe fn mark_smallmap(&mut self, idx: u32) {
        self.smallmap |= 1 << idx;
    }

    unsafe fn clear_smallmap(&mut self, idx: u32) {
        self.smallmap &= !(1 << idx);
    }

    unsafe fn treemap_is_marked(&self, idx: u32) -> bool {
        self.treemap & (1 << idx) != 0
    }

    unsafe fn mark_treemap(&mut self, idx: u32) {
        self.treemap |= 1 << idx;
    }

    unsafe fn clear_treemap(&mut self, idx: u32) {
        self.treemap &= !(1 << idx);
    }

    unsafe fn unlink_chunk(&mut self, chunk: *mut Chunk, size: usize) {
        if self.is_small(size) {
            self.unlink_small_chunk(chunk, size)
        } else {
            self.unlink_large_chunk(chunk as *mut TreeChunk);
        }
    }

    unsafe fn unlink_small_chunk(&mut self, chunk: *mut Chunk, size: usize) {
        let f = (*chunk).prev;
        let b = (*chunk).next;
        let idx = self.small_index(size);
        assert!(chunk != b);
        assert!(chunk != f);
        assert_eq!(Chunk::size(chunk), self.small_index2size(idx));
        if b == f {
            self.clear_smallmap(idx);
        } else {
            (*f).next = b;
            (*b).prev = f;
        }
    }

    unsafe fn unlink_large_chunk(&mut self, chunk: *mut TreeChunk) {
        let xp = (*chunk).parent;
        let mut r;
        if TreeChunk::next(chunk) != chunk {
            let f = TreeChunk::prev(chunk);
            r = TreeChunk::next(chunk);
            (*f).chunk.next = TreeChunk::chunk(r);
            (*r).chunk.prev = TreeChunk::chunk(f);
        } else {
            let mut rp = &mut (*chunk).child[1];
            if rp.is_null() {
                rp = &mut (*chunk).child[0];
            }
            r = *rp;
            if !rp.is_null() {
                loop {
                    let mut cp = &mut (**rp).child[1];
                    if cp.is_null() {
                        cp = &mut (**rp).child[0];
                    }
                    if cp.is_null() {
                        break
                    }
                    rp = cp;
                }
                r = *rp;
                *rp = ptr::null_mut();
            }
        }

        if xp.is_null() {
            return
        }

        let h = self.treebin_at((*chunk).index);
        if chunk == *h {
            *h = r;
            if r.is_null() {
                self.clear_treemap((*chunk).index);
            }
        } else {
            if (*xp).child[0] == chunk {
                (*xp).child[0] = r;
            } else {
                (*xp).child[1] = r;
            }
        }

        if !r.is_null() {
            (*r).parent = xp;
            let c0 = (*chunk).child[0];
            if !c0.is_null() {
                (*r).child[0] = c0;
                (*c0).parent = r;
            }
            let c1 = (*chunk).child[1];
            if !c1.is_null() {
                (*r).child[1] = c1;
                (*c1).parent = r;
            }
        }
    }

    unsafe fn free(&mut self, mem: *mut u8) {
        self.check_malloc_state();

        let mut p = Chunk::from_mem(mem);
        let mut psize = Chunk::size(p);
        let next = Chunk::plus_offset(p, psize);
        if !Chunk::pinuse(p) {
            let prevsize = (*p).prev_foot;

            // if Chunk::mmapped(p) {
            // }

            let prev = Chunk::minus_offset(p, prevsize);
            psize += prevsize;
            p = prev;
            if p != self.dv {
                self.unlink_chunk(p, prevsize);
            } else if (*next).head & INUSE == INUSE {
                self.dvsize = psize;
                Chunk::set_free_with_pinuse(p, psize, next);
                return
            }
        }

        // Consolidate forward if we can
        if !Chunk::cinuse(next) {
            if next == self.top {
                self.topsize += psize;
                let tsize = self.topsize;
                self.top = p;
                (*p).head = tsize | PINUSE;
                if p == self.dv {
                    self.dv = ptr::null_mut();
                    self.dvsize = 0;
                }
                if self.should_trim(tsize) {
                    self.sys_trim(0);
                }
                return
            } else if next == self.dv {
                self.dvsize += psize;
                let dsize = self.dvsize;
                self.dv = p;
                Chunk::set_size_and_pinuse_of_free_chunk(p, dsize);
                return
            } else {
                let nsize = Chunk::size(next);
                psize += nsize;
                self.unlink_chunk(next, nsize);
                Chunk::set_size_and_pinuse_of_free_chunk(p, psize);
                if p == self.dv {
                    self.dvsize = psize;
                    return
                }
            }
        } else {
            Chunk::set_free_with_pinuse(p, psize, next);
        }

        if self.is_small(psize) {
            self.insert_small_chunk(p, psize);
            self.check_free_chunk(p);
        } else {
            self.insert_large_chunk(p as *mut TreeChunk, psize);
            self.check_free_chunk(p);
            self.release_checks -= 1;
            if self.release_checks == 0 {
                self.release_unused_segments();
            }
        }
    }

    fn should_trim(&self, size: usize) -> bool {
        size > self.trim_check
    }

    unsafe fn sys_trim(&mut self, mut pad: usize) -> bool {
        let mut released = 0;
        if pad < self.max_request() && !self.top.is_null() {
            pad += self.top_foot_size();
            if self.topsize > pad {
                let unit = DEFAULT_GRANULARITY;
                let extra = ((self.topsize - pad + unit - 1) / unit - 1) * unit;
                let sp = self.segment_holding(self.top as *mut u8);
                assert!(!sp.is_null());

                if !Segment::is_extern(sp) {
                    if Segment::can_release_part(sp) {
                        if (*sp).size >= extra && !self.has_segment_link(sp) {
                            let newsize = (*sp).size - extra;
                            if sys::free_part((*sp).base, (*sp).size, newsize) {
                                released = extra;
                            }
                        }
                    }
                }

                if released != 0 {
                    (*sp).size -= released;
                    self.footprint -= released;
                    let top = self.top;
                    let topsize = self.topsize - released;
                    self.init_top(top, topsize);
                    self.check_top_chunk(self.top);
                }
            }

            released += self.release_unused_segments();

            if released == 0 && self.topsize > self.trim_check {
                self.trim_check = usize::max_value();
            }
        }

        released != 0
    }

    unsafe fn has_segment_link(&self, ptr: *mut Segment) -> bool {
        let mut sp = &self.seg as *const Segment as *mut Segment;
        while !sp.is_null() {
            if Segment::holds(ptr, sp as *mut u8) {
                return true
            }
            sp = (*sp).next;
        }
        false
    }

    /// Unmap and unlink any mapped segments that don't contain used chunks
    unsafe fn release_unused_segments(&mut self) -> usize {
        let mut released = 0;
        let mut nsegs = 0;
        let mut pred = &mut self.seg as *mut Segment;
        let mut sp = (*pred).next;
        while !sp.is_null() {
            let base = (*sp).base;
            let size = (*sp).size;
            let next = (*sp).next;
            nsegs += 1;

            if Segment::can_release_part(sp) && !Segment::is_extern(sp) {
                let p = self.align_as_chunk(base);
                let psize = Chunk::size(p);
                // We can unmap if the first chunk holds the entire segment and
                // isn't pinned.
                let chunk_top = (p as *mut u8).offset(psize as isize);
                let top = base.offset((size - self.top_foot_size()) as isize);
                if !Chunk::inuse(p) && chunk_top >= top {
                    let tp = p as *mut TreeChunk;
                    assert!(Segment::holds(sp, sp as *mut u8));
                    if p == self.dv {
                        self.dv = ptr::null_mut();
                        self.dvsize = 0;
                    } else {
                        self.unlink_large_chunk(tp);
                    }
                    if sys::free(base, size) {
                        released += size;
                        self.footprint -= size;
                        // unlink our obsolete record
                        sp = pred;
                        (*sp).next = next;
                    } else {
                        // back out if we can't unmap
                        self.insert_large_chunk(tp, psize);
                    }
                }
            }
            pred = sp;
            sp = next;
        }
        self.release_checks = if nsegs > MAX_RELEASE_CHECK_RATE {
            nsegs
        } else {
            MAX_RELEASE_CHECK_RATE
        };
        return released
    }

    // Sanity checks

    unsafe fn check_any_chunk(&self, p: *mut Chunk) {
        assert!(self.is_aligned(Chunk::to_mem(p) as usize) ||
                (*p).head == Chunk::fencepost_head());
        assert!(p as *mut u8 >= self.least_addr);
    }

    unsafe fn check_top_chunk(&self, p: *mut Chunk) {
        let sp = self.segment_holding(p as *mut u8);
        let sz = (*p).head & !INUSE;
        assert!(!sp.is_null());
        assert!(self.is_aligned(Chunk::to_mem(p) as usize) ||
                (*p).head == Chunk::fencepost_head());
        assert!(p as *mut u8 >= self.least_addr);
        assert_eq!(sz, self.topsize);
        assert!(sz > 0);
        assert_eq!(sz, (*sp).base as usize + (*sp).size - p as usize - self.top_foot_size());
        assert!(Chunk::pinuse(p));
        assert!(!Chunk::pinuse(Chunk::plus_offset(p, sz)));
    }

    unsafe fn check_malloced_chunk(&self, mem: *mut u8, s: usize) {
        if mem.is_null() {
            return
        }
        let p = Chunk::from_mem(mem);
        let sz = (*p).head & !INUSE;
        self.check_inuse_chunk(p);
        assert_eq!(align_up(sz, self.malloc_alignment()), sz);
        assert!(sz >= self.min_chunk_size());
        assert!(sz >= s);
        // assert!( // something about mmap?
    }

    unsafe fn check_inuse_chunk(&self, p: *mut Chunk) {
        self.check_any_chunk(p);
        assert!(Chunk::inuse(p));
        assert!(Chunk::pinuse(Chunk::next(p)));
        // something about mmap?
    }

    unsafe fn check_free_chunk(&self, p: *mut Chunk) {
        let sz = Chunk::size(p);
        let next = Chunk::plus_offset(p, sz);
        self.check_any_chunk(p);
        assert!(!Chunk::inuse(p));
        assert!(!Chunk::pinuse(Chunk::next(p)));
        // assert!(!Chunk::is_mmapped(p));
        if p != self.dv && p != self.top {
            if sz >= self.min_chunk_size() {
                assert_eq!(align_up(sz, self.malloc_alignment()), sz);
                assert!(self.is_aligned(Chunk::to_mem(p) as usize));
                assert_eq!((*next).prev_foot, sz);
                assert!(Chunk::pinuse(p));
                assert!(next == self.top || Chunk::inuse(next));
                assert_eq!((*(*p).next).prev, p);
                assert_eq!((*(*p).prev).next, p);
            } else {
                assert_eq!(sz, mem::size_of::<usize>());
            }
        }
    }

    unsafe fn check_malloc_state(&mut self) {
        for i in 0..NSMALLBINS {
            self.check_smallbin(i as u32);
        }
        for i in 0..NTREEBINS {
            self.check_treebin(i as u32);
        }
        if self.dvsize != 0 {
            self.check_any_chunk(self.dv);
            assert_eq!(self.dvsize, Chunk::size(self.dv));
            assert!(self.dvsize >= self.min_chunk_size());
            let dv = self.dv;
            assert!(!self.bin_find(dv));
        }
        if !self.top.is_null() {
            self.check_top_chunk(self.top);
            assert!(self.topsize > 0);
            let top = self.top;
            assert!(!self.bin_find(top));
        }
        let total = self.traverse_and_check();
        assert!(total <= self.footprint);
        assert!(self.footprint <= self.max_footprint);
    }

    unsafe fn check_smallbin(&mut self, idx: u32) {
        let b = self.smallbin_at(idx);
        let mut p = (*b).next;
        let empty = self.smallmap & (1 << idx) == 0;
        if p == b {
            assert!(empty)
        }
        if !empty {
            while p != b {
                let size = Chunk::size(p);
                self.check_free_chunk(p);
                assert_eq!(self.small_index(size), idx);
                assert!((*p).next == b ||
                        Chunk::size((*p).next) == Chunk::size(p));
                let q = Chunk::next(p);
                if (*q).head != Chunk::fencepost_head() {
                    self.check_inuse_chunk(q);
                }
                p = (*p).next;
            }
        }
    }

    unsafe fn check_treebin(&mut self, idx: u32) {
        let tb = self.treebin_at(idx);
        let t = *tb;
        let empty = self.treemap & (1 << idx) == 0;
        if t.is_null() {
            assert!(empty);
        }
        if !empty {
            self.check_tree(t);
        }
    }

    unsafe fn check_tree(&mut self, t: *mut TreeChunk) {
        let tc = TreeChunk::chunk(t);
        let tindex = (*t).index;
        let tsize = Chunk::size(tc);
        let idx = self.compute_tree_index(tsize);
        assert_eq!(tindex, idx);
        assert!(tsize >= self.min_large_size());
        assert!(tsize >= self.min_size_for_tree_index(idx));
        assert!(idx == NTREEBINS as u32 - 1 ||
                tsize < self.min_size_for_tree_index(idx + 1));

        let mut u = t;
        let mut head = ptr::null_mut();
        loop {
            let uc = TreeChunk::chunk(u);
            self.check_any_chunk(uc);
            assert_eq!((*u).index, tindex);
            assert_eq!(Chunk::size(uc), tsize);
            assert!(!Chunk::inuse(uc));
            assert!(!Chunk::pinuse(Chunk::next(uc)));
            assert_eq!((*(*uc).next).prev, uc);
            assert_eq!((*(*uc).prev).next, uc);
            let left = (*u).child[0];
            let right = (*u).child[1];
            if (*u).parent.is_null() {
                assert!(left.is_null());
                assert!(right.is_null());
            } else {
                assert!(head.is_null());
                head = u;
                assert!((*u).parent != u);
                assert!((*(*u).parent).child[0] == u ||
                        (*(*u).parent).child[1] == u ||
                        *((*u).parent as *mut *mut TreeChunk) == u);
                if !left.is_null() {
                    assert_eq!((*left).parent, u);
                    assert!(left != u);
                    self.check_tree(left);
                }
                if !right.is_null() {
                    assert_eq!((*right).parent, u);
                    assert!(right != u);
                    self.check_tree(right);
                }
                if !left.is_null() && !right.is_null() {
                    assert!(Chunk::size(TreeChunk::chunk(left)) <
                            Chunk::size(TreeChunk::chunk(right)));
                }
            }

            u = TreeChunk::prev(u);
            if u == t {
                break
            }
        }
        assert!(!head.is_null());
    }

    fn min_size_for_tree_index(&self, idx: u32) -> usize {
        let idx = idx as usize;
        (1 << ((idx >> 1) + TREEBIN_SHIFT)) |
            ((idx & 1) << ((idx >> 1) + TREEBIN_SHIFT - 1))
    }

    unsafe fn bin_find(&mut self, chunk: *mut Chunk) -> bool {
        let size = Chunk::size(chunk);
        if self.is_small(size) {
            let sidx = self.small_index(size);
            let b = self.smallbin_at(sidx);
            if !self.smallmap_is_marked(sidx) {
                return false
            }
            let mut p = b;
            loop {
                if p == chunk {
                    return true
                }
                p = (*p).prev;
                if p == b {
                    return false
                }
            }
        } else {
            let tidx = self.compute_tree_index(size);
            if !self.treemap_is_marked(tidx) {
                return false
            }
            let mut t = *self.treebin_at(tidx);
            let mut sizebits = size << leftshift_for_tree_index(tidx);
            while !t.is_null() && Chunk::size(TreeChunk::chunk(t)) != size {
                t = (*t).child[(sizebits >> (mem::size_of::<usize>() * 8 - 1)) & 1];
                sizebits <<= 1;
            }
            if t.is_null() {
                return false
            }
            let mut u = t;
            let chunk = chunk as *mut TreeChunk;
            loop {
                if u == chunk {
                    return true
                }
                u = TreeChunk::prev(u);
                if u == t {
                    return false
                }
            }
        }

    }

    unsafe fn traverse_and_check(&self) -> usize {
        0
    }
}

const PINUSE: usize = 1 << 0;
const CINUSE: usize = 1 << 1;
const FLAG4: usize = 1 << 2;
const INUSE: usize = PINUSE | CINUSE;
const FLAG_BITS: usize = PINUSE | CINUSE | FLAG4;

impl Chunk {
    unsafe fn fencepost_head() -> usize {
        INUSE | mem::size_of::<usize>()
    }

    unsafe fn size(me: *mut Chunk) -> usize {
        (*me).head & !FLAG_BITS
    }

    unsafe fn next(me: *mut Chunk) -> *mut Chunk {
        (me as *mut u8).offset(((*me).head & !FLAG_BITS) as isize) as *mut Chunk
    }

    unsafe fn prev(me: *mut Chunk) -> *mut Chunk {
        (me as *mut u8).offset((*me).prev_foot as isize) as *mut Chunk
    }

    unsafe fn cinuse(me: *mut Chunk) -> bool {
        (*me).head & CINUSE != 0
    }

    unsafe fn pinuse(me: *mut Chunk) -> bool {
        (*me).head & PINUSE != 0
    }

    unsafe fn clear_pinuse(me: *mut Chunk) {
        (*me).head &= !PINUSE;
    }

    unsafe fn inuse(me: *mut Chunk) -> bool {
        (*me).head & INUSE != PINUSE
    }

    // TODO: is this right?
    unsafe fn mmapped(me: *mut Chunk) -> bool {
        (*me).head & INUSE == 0
    }

    unsafe fn set_inuse_and_pinuse(me: *mut Chunk, size: usize) {
        (*me).head = PINUSE | size | CINUSE;
        let next = Chunk::plus_offset(me, size);
        (*next).head |= PINUSE;
    }

    unsafe fn set_size_and_pinuse_of_inuse_chunk(me: *mut Chunk, size: usize) {
        (*me).head = size | PINUSE | CINUSE;
    }

    unsafe fn set_size_and_pinuse_of_free_chunk(me: *mut Chunk, size: usize) {
        (*me).head = size | PINUSE;
        Chunk::set_foot(me, size);
    }

    unsafe fn set_free_with_pinuse(p: *mut Chunk, size: usize, n: *mut Chunk) {
        Chunk::clear_pinuse(n);
        Chunk::set_size_and_pinuse_of_free_chunk(p, size);
    }

    unsafe fn set_foot(me: *mut Chunk, size: usize) {
        let next = Chunk::plus_offset(me, size);
        (*next).prev_foot = size;
    }

    unsafe fn plus_offset(me: *mut Chunk, offset: usize) -> *mut Chunk {
        (me as *mut u8).offset(offset as isize) as *mut Chunk
    }

    unsafe fn minus_offset(me: *mut Chunk, offset: usize) -> *mut Chunk {
        (me as *mut u8).offset(-(offset as isize)) as *mut Chunk
    }

    unsafe fn to_mem(me: *mut Chunk) -> *mut u8 {
        (me as *mut u8).offset(2 * (mem::size_of::<usize>() as isize))
    }

    unsafe fn from_mem(mem: *mut u8) -> *mut Chunk {
        mem.offset(-2 * (mem::size_of::<usize>() as isize)) as *mut Chunk
    }
}

impl TreeChunk {
    unsafe fn leftmost_child(me: *mut TreeChunk) -> *mut TreeChunk {
        let left = (*me).child[0];
        if left.is_null() {
            (*me).child[1]
        } else {
            left
        }
    }

    unsafe fn chunk(me: *mut TreeChunk) -> *mut Chunk {
        &mut (*me).chunk
    }

    unsafe fn next(me: *mut TreeChunk) -> *mut TreeChunk {
        (*TreeChunk::chunk(me)).next as *mut TreeChunk
    }

    unsafe fn prev(me: *mut TreeChunk) -> *mut TreeChunk {
        (*TreeChunk::chunk(me)).prev as *mut TreeChunk
    }
}

const EXTERN: u32 = 1 << 0;

impl Segment {
    unsafe fn is_extern(seg: *mut Segment) -> bool {
        (*seg).flags & EXTERN != 0
    }

    unsafe fn can_release_part(seg: *mut Segment) -> bool {
        sys::can_release_part((*seg).flags >> 1)
    }

    unsafe fn sys_flags(seg: *mut Segment) -> u32 {
        (*seg).flags >> 1
    }

    unsafe fn holds(seg: *mut Segment, addr: *mut u8) -> bool {
        (*seg).base <= addr && addr < Segment::top(seg)
    }

    unsafe fn top(seg: *mut Segment) -> *mut u8 {
        (*seg).base.offset((*seg).size as isize)
    }
}

#[repr(C)]
struct Header(*mut u8);

unsafe fn get_header<'a>(ptr: *mut u8) -> &'a mut Header {
    &mut *(ptr as *mut Header).offset(-1)
}

unsafe fn align_ptr(ptr: *mut u8, align: usize) -> *mut u8 {
    let aligned = ptr.offset((align - (ptr as usize & (align - 1))) as isize);
    *get_header(aligned) = Header(ptr);
    aligned
}

unsafe impl Alloc for Dlmalloc {
    #[inline]
    unsafe fn alloc(&mut self, layout: Layout) -> Result<*mut u8, AllocErr> {
        let ptr = if layout.align() <= self.malloc_alignment() {
            self.malloc(layout.size())
        } else {
            let size = layout.size() + layout.align();
            let ptr = self.malloc(size);
            if ptr.is_null() {
                ptr
            } else {
                align_ptr(ptr, layout.align())
            }
        };
        self.check_malloc_state();
        if ptr.is_null() {
            Err(AllocErr::Exhausted { request: layout })
        } else {
            Ok(ptr)
        }
    }

    // #[inline]
    // unsafe fn alloc_zeroed(&mut self, layout: Layout)
    //     -> Result<*mut u8, AllocErr>
    // {
    //     (&*self).alloc_zeroed(layout)
    // }
    //
    #[inline]
    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        if layout.align() <= self.malloc_alignment() {
            self.free(ptr)
        } else {
            let header = get_header(ptr);
            self.free(header.0)
        }
        self.check_malloc_state();
    }

    // #[inline]
    // unsafe fn realloc(&mut self,
    //                   ptr: *mut u8,
    //                   old_layout: Layout,
    //                   new_layout: Layout) -> Result<*mut u8, AllocErr> {
    //     (&*self).realloc(ptr, old_layout, new_layout)
    // }

    // fn oom(&mut self, err: AllocErr) -> ! {
    //     System.oom(err)
    // }

    // #[inline]
    // fn usable_size(&self, layout: &Layout) -> (usize, usize) {
    //     (&self).usable_size(layout)
    // }
    //
    // #[inline]
    // unsafe fn alloc_excess(&mut self, layout: Layout) -> Result<Excess, AllocErr> {
    //     (&*self).alloc_excess(layout)
    // }
    //
    // #[inline]
    // unsafe fn realloc_excess(&mut self,
    //                          ptr: *mut u8,
    //                          layout: Layout,
    //                          new_layout: Layout) -> Result<Excess, AllocErr> {
    //     (&*self).realloc_excess(ptr, layout, new_layout)
    // }
    //
    // #[inline]
    // unsafe fn grow_in_place(&mut self,
    //                         ptr: *mut u8,
    //                         layout: Layout,
    //                         new_layout: Layout) -> Result<(), CannotReallocInPlace> {
    //     (&*self).grow_in_place(ptr, layout, new_layout)
    // }
    //
    // #[inline]
    // unsafe fn shrink_in_place(&mut self,
    //                           ptr: *mut u8,
    //                           layout: Layout,
    //                           new_layout: Layout) -> Result<(), CannotReallocInPlace> {
    //     (&*self).shrink_in_place(ptr, layout, new_layout)
    // }
}

// unsafe impl<'a> Alloc for &'a Dlmalloc {
//     #[inline]
//     unsafe fn alloc(&mut self, layout: Layout) -> Result<*mut u8, AllocErr> {
//         panic!()
//     }
//
//     // #[inline]
//     // unsafe fn alloc_zeroed(&mut self, layout: Layout)
//     //     -> Result<*mut u8, AllocErr>
//     // {
//     //     panic!()
//     // }
//
//     #[inline]
//     unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
//         panic!()
//     }
//
//     // #[inline]
//     // unsafe fn realloc(&mut self,
//     //                   ptr: *mut u8,
//     //                   old_layout: Layout,
//     //                   new_layout: Layout) -> Result<*mut u8, AllocErr> {
//     //     panic!()
//     // }
//
//     fn oom(&mut self, err: AllocErr) -> ! {
//         System.oom(err)
//     }
// }