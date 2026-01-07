use std::{
    ops::{Index, IndexMut},
    ptr,
    sync::atomic::{AtomicPtr, AtomicU16, AtomicU32, AtomicUsize, Ordering},
};

pub type Idx = u32;
pub type AtomicIdx = AtomicU32;
pub const IDX_HALF_BITS: usize = Idx::BITS as usize >> 1;
pub const MAX_GROWS: usize = IDX_HALF_BITS;
pub const MAX_SEGMENTS: usize = IDX_HALF_BITS + 1;
pub const SEGMENT_BASE_SIZE: usize = u16::MAX as usize + 1;

pub type Layer1<T> = [Layer2<T>; MAX_SEGMENTS];
pub type Layer2<T> = AtomicPtr<T>;
const NIL: Idx = Idx::MAX;

#[derive(Debug)]
pub struct Node {
    pub ref_cnt: AtomicU32,
    pub level: u32,
    pub low: Idx,
    pub high: Idx,
}

/// A lock-free, growable node allocator implementation.
///
/// ## implementation
/// 1. A node store is 2-layer hierarchy. The higher layer is initialied with 17 pointers, while
///    the lower layer is lazily allocated to double the size. The maximum number of nodes of a
///    node store is [u32::MAX].
///
/// ### alloc/free
/// 1. ptr in `links` points to the next free node.
/// 2. alloc: use CAS to get `head`, replace it with links[head].load()
/// 3. free: use CAS to put `idx` to `head`, place `head` using links[idx].store()
/// 4. if the used_cnt reaches the capacity, use CAS to grow a new Layer2
///
pub struct NodeStore {
    // nodes list
    nodes: Layer1<Node>,

    // links list
    links: Layer1<AtomicIdx>,

    // free head
    head: AtomicIdx,

    // the count of used nodes, free_cnt is `u32::MAX - used_cnt`
    used_cnt: AtomicUsize,

    // number of layer2, if `used_cnt == capacity (SEGMENT_BASE_SIZE * 2^num_grows)`, it needs a grow
    num_grows: AtomicUsize,
}

pub trait Allocator:
    Send + Sync + Index<Idx, Output = Node> + IndexMut<Idx, Output = Node>
{
    fn alloc(&self, level: u32, low: Idx, high: Idx) -> Idx;
    fn free(&self, idx: Idx);
}

impl NodeStore {
    #[inline]
    fn new_layer1<T>() -> Layer1<T> {
        std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut()))
    }

    #[inline]
    fn segment_size(seg_idx: usize) -> usize {
        SEGMENT_BASE_SIZE << (seg_idx.saturating_sub(1))
    }

    #[inline]
    fn capacity_for(num_grows: usize) -> usize {
        (SEGMENT_BASE_SIZE) << num_grows
    }

    #[inline]
    fn locate(idx: Idx) -> (usize, usize) {
        let hi = (idx >> IDX_HALF_BITS) as u16;
        let seg_idx = IDX_HALF_BITS - hi.isolate_highest_one().leading_zeros() as usize;

        (
            seg_idx,
            (idx & (Idx::MAX >> (IDX_HALF_BITS - seg_idx.saturating_sub(1)))) as usize,
        )
    }

    #[inline]
    fn node_ptr(&self, idx: Idx) -> *mut Node {
        let (seg_idx, offset) = Self::locate(idx);
        let base = self.nodes[seg_idx].load(Ordering::Acquire);
        debug_assert!(!base.is_null(), "node segment {seg_idx} is not allocated");
        unsafe { base.add(offset) }
    }

    #[inline]
    fn link_ptr(&self, idx: Idx) -> *mut AtomicIdx {
        let (seg_idx, offset) = Self::locate(idx);
        let base = self.links[seg_idx].load(Ordering::Acquire);
        debug_assert!(!base.is_null(), "link segment {seg_idx} is not allocated");
        unsafe { base.add(offset) }
    }

    fn grow_if_needed(&self) {
        loop {
            std::thread::current_id()
            let num_grows = self.num_grows.load(Ordering::Acquire);
            let capacity = Self::capacity_for(num_grows);
            if self.used_cnt.load(Ordering::Acquire) < capacity {
                return;
            }
            if num_grows + 1 >= MAX_GROWS {
                return;
            }
            if self
                .num_grows
                .compare_exchange(
                    num_grows,
                    num_grows + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                let seg_idx = num_grows + 1;
                let seg_size = capacity;
                let start_idx = capacity;
                let nodes_ptr = Self::alloc_nodes(seg_size);
                let links_ptr = Self::alloc_links(seg_size);

                self.nodes[seg_idx].store(nodes_ptr, Ordering::Release);
                self.links[seg_idx].store(links_ptr, Ordering::Release);

                for i in 0..seg_size - 1 {
                    let next = start_idx + i + 1;
                    unsafe { (*links_ptr.add(i)).store(next as Idx, Ordering::Relaxed) };
                }

                let mut head = self.head.load(Ordering::Acquire);
                loop {
                    unsafe {
                        (*links_ptr.add(seg_size - 1)).store(head, Ordering::Release);
                    }
                    match self.head.compare_exchange_weak(
                        head,
                        start_idx as Idx,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => break,
                        Err(updated) => head = updated,
                    }
                }

                return;
            }
        }
    }

    fn alloc_nodes(size: usize) -> *mut Node {
        let mut nodes = Vec::with_capacity(size);
        for _ in 0..size {
            nodes.push(Node {
                ref_cnt: AtomicU32::new(0),
                level: 0,
                low: 0,
                high: 0,
            });
        }
        Box::into_raw(nodes.into_boxed_slice()) as *mut Node
    }

    fn alloc_links(size: usize) -> *mut AtomicIdx {
        let mut links = Vec::with_capacity(size);
        for _ in 0..size {
            links.push(AtomicU32::new(NIL));
        }
        Box::into_raw(links.into_boxed_slice()) as *mut AtomicIdx
    }
}

impl Index<Idx> for NodeStore {
    type Output = Node;

    fn index(&self, idx: Idx) -> &Self::Output {
        unsafe { &*self.node_ptr(idx) }
    }
}

impl IndexMut<Idx> for NodeStore {
    fn index_mut(&mut self, idx: Idx) -> &mut Self::Output {
        unsafe { &mut *self.node_ptr(idx) }
    }
}

impl Allocator for NodeStore {
    fn alloc(&self, level: u32, low: Idx, high: Idx) -> Idx {
        loop {
            let head = self.head.load(Ordering::Acquire);
            if head == NIL {
                self.grow_if_needed();
                let head = self.head.load(Ordering::Acquire);
                if head == NIL {
                    let num_grows = self.num_grows.load(Ordering::Acquire);
                    let capacity = Self::capacity_for(num_grows);
                    if self.used_cnt.load(Ordering::Acquire) >= capacity
                        && num_grows + 1 >= MAX_GROWS
                    {
                        panic!("node store reached capacity {capacity}");
                    }
                }
                continue;
            }
            let next = unsafe { (*self.link_ptr(head)).load(Ordering::Acquire) };
            if self
                .head
                .compare_exchange_weak(head, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.used_cnt.fetch_add(1, Ordering::AcqRel);
                let node = unsafe { &mut *self.node_ptr(head) };
                node.ref_cnt.store(0, Ordering::Relaxed);
                node.level = level;
                node.low = low;
                node.high = high;
                return head;
            }
        }
    }

    fn free(&self, idx: Idx) {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            unsafe { (*self.link_ptr(idx)).store(head, Ordering::Release) };
            match self
                .head
                .compare_exchange_weak(head, idx, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    self.used_cnt.fetch_sub(1, Ordering::AcqRel);
                    return;
                }
                Err(updated) => head = updated,
            }
        }
    }
}

impl Drop for NodeStore {
    fn drop(&mut self) {
        for seg_idx in 0..MAX_SEGMENTS {
            let node_ptr = self.nodes[seg_idx].load(Ordering::Relaxed);
            if !node_ptr.is_null() {
                let seg_size = Self::segment_size(seg_idx);
                unsafe {
                    let slice = std::slice::from_raw_parts_mut(node_ptr, seg_size);
                    drop(Box::from_raw(slice));
                }
            }

            let link_ptr = self.links[seg_idx].load(Ordering::Relaxed);
            if !link_ptr.is_null() {
                let seg_size = Self::segment_size(seg_idx);
                unsafe {
                    let slice = std::slice::from_raw_parts_mut(link_ptr, seg_size);
                    drop(Box::from_raw(slice));
                }
            }
        }
    }
}

impl Default for NodeStore {
    // initialied with capacity SEGMENT_BASE_SIZE
    fn default() -> Self {
        let nodes = Self::new_layer1();
        let links = Self::new_layer1();

        let seg_size = SEGMENT_BASE_SIZE;
        let nodes_ptr = Self::alloc_nodes(seg_size);
        let links_ptr = Self::alloc_links(seg_size);

        for i in 0..seg_size - 1 {
            unsafe { (*links_ptr.add(i)).store((i as u32) + 1, Ordering::Relaxed) };
        }
        unsafe { (*links_ptr.add(seg_size - 1)).store(NIL, Ordering::Relaxed) };

        nodes[0].store(nodes_ptr, Ordering::Release);
        links[0].store(links_ptr, Ordering::Release);

        Self {
            nodes,
            links,
            head: AtomicU32::new(0),
            used_cnt: AtomicUsize::new(0),
            num_grows: AtomicUsize::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_segment_size() {
        assert_eq!(NodeStore::segment_size(0), SEGMENT_BASE_SIZE);
        assert_eq!(NodeStore::segment_size(1), SEGMENT_BASE_SIZE);
        assert_eq!(NodeStore::segment_size(2), SEGMENT_BASE_SIZE << 1);
        assert_eq!(NodeStore::segment_size(3), SEGMENT_BASE_SIZE << 2);
        assert_eq!(NodeStore::segment_size(15), SEGMENT_BASE_SIZE << 14);
        assert_eq!(NodeStore::segment_size(16), SEGMENT_BASE_SIZE << 15);
    }

    #[test]
    fn test_capacity_for() {
        assert_eq!(NodeStore::capacity_for(0), SEGMENT_BASE_SIZE);
        assert_eq!(NodeStore::capacity_for(1), SEGMENT_BASE_SIZE << 1);
        assert_eq!(NodeStore::capacity_for(2), SEGMENT_BASE_SIZE << 2);
        assert_eq!(NodeStore::capacity_for(15), SEGMENT_BASE_SIZE << 15);
        assert_eq!(NodeStore::capacity_for(16), SEGMENT_BASE_SIZE << 16);
    }

    #[test]
    fn test_locate() {
        assert_eq!(NodeStore::locate(0), (0, 0));
        assert_eq!(
            NodeStore::locate(SEGMENT_BASE_SIZE as Idx - 1),
            (0, NodeStore::segment_size(0) - 1)
        );
        assert_eq!(NodeStore::locate(SEGMENT_BASE_SIZE as Idx), (1, 0));
        assert_eq!(
            NodeStore::locate((SEGMENT_BASE_SIZE << 1) as Idx - 1),
            (1, NodeStore::segment_size(1) - 1)
        );
        assert_eq!(NodeStore::locate((SEGMENT_BASE_SIZE << 1) as Idx), (2, 0));
        assert_eq!(
            NodeStore::locate((SEGMENT_BASE_SIZE << 2) as Idx - 1),
            (2, NodeStore::segment_size(2) - 1)
        );
        // check from (15, 0)
        assert_eq!(NodeStore::locate((SEGMENT_BASE_SIZE << 14) as Idx), (15, 0));
        assert_eq!(
            NodeStore::locate((SEGMENT_BASE_SIZE << 15) as Idx - 1),
            (15, NodeStore::segment_size(15) - 1)
        );
        assert_eq!(NodeStore::locate((SEGMENT_BASE_SIZE << 15) as Idx), (16, 0));
        // maximum idx
        assert_eq!(
            NodeStore::locate(NIL),
            (16, NodeStore::segment_size(16) - 1)
        );
    }
}
