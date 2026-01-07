use std::ops::{Deref, DerefMut};

use crate::node::{Idx, Node};

///// A lock-free, growable node allocator implementation.
//#[derive(Default, Debug)]
//pub struct NodeStore {
//    slab: Slab<Node<usize>, DefaultConfig>,
//}

pub trait Allocator<I: Idx>: Copy + Send + Sync + Default {
    fn alloc(&self, node: Node<I, Self>) -> I;
    fn free(&self, idx: I);
    fn index(&self, idx: I) -> impl Deref<Target = Node<I, Self>>;
    fn index_mut(&self, idx: I) -> impl DerefMut<Target = Node<I, Self>>;
}

// impl Allocator<usize> for NodeStore {
//     fn alloc(&self, node: Node<usize>) -> usize {
//         self.slab.insert(node).unwrap()
//     }
//
//     fn free(&self, idx: usize) {
//         self.slab.remove(idx);
//     }
//
//     #[inline]
//     fn index(&self, idx: usize) -> impl Deref<Target = Node<usize>> {
//         self.slab.get(idx).unwrap()
//     }
//
//     #[inline]
//     fn index_mut(&self, idx: usize) -> impl DerefMut<Target = Node<usize>> {
//         self.slab.get(idx).unwrap()
//     }
// }

impl Allocator<usize> for () {
    #[inline]
    fn alloc(&self, node: Node<usize, Self>) -> usize {
        Box::leak(Box::new(node)) as *mut Node<usize, Self> as usize
    }

    #[inline]
    fn free(&self, idx: usize) {
        unsafe {
            let _ = Box::from_raw(idx as *mut Node<usize, Self>);
        }
    }

    #[inline]
    fn index(&self, idx: usize) -> impl Deref<Target = Node<usize, Self>> {
        unsafe { &mut *(idx as *mut Node<usize, Self>) as &mut Node<usize, Self> }
    }

    #[inline]
    fn index_mut(&self, idx: usize) -> impl DerefMut<Target = Node<usize, Self>> {
        unsafe { &mut *(idx as *mut Node<usize, Self>) as &mut Node<usize, Self> }
    }
}
