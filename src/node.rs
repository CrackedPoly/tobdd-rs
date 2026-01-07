use std::{
    fmt::Debug,
    hash::Hash,
    sync::atomic::{AtomicPrimitive, AtomicPtr, AtomicUsize, Ordering},
};

use funty::{AtLeast32, Unsigned};

use crate::alloc::Allocator;

pub trait Idx: Unsigned + AtLeast32 + AtomicPrimitive {
    const NULL: Self;
}
impl Idx for usize {
    const NULL: Self = usize::MIN;
}

pub struct Node<I: Idx, A: Allocator<I>> {
    pub ref_cnt: AtomicUsize,
    pub level: I,
    pub low: I,
    pub high: I,
    pub next: NodePtr<I, A>,
}

impl<I: Idx, A: Allocator<I>> Node<I, A> {
    #[inline]
    pub fn key(&self) -> (I, I, I) {
        (self.level, self.low, self.high)
    }
}

impl<A: Allocator<usize>> Default for Node<usize, A> {
    fn default() -> Self {
        Self {
            ref_cnt: Default::default(),
            level: Default::default(),
            low: Default::default(),
            high: Default::default(),
            next: Default::default(),
        }
    }
}

impl<A: Allocator<usize>> Node<usize, A> {
    pub fn from(level: usize, low: usize, high: usize) -> Self {
        Node {
            ref_cnt: AtomicUsize::new(0),
            level,
            low,
            high,
            next: NodePtr::default(),
        }
    }
}

pub(crate) struct NodePtr<I: Idx, A: Allocator<I>> {
    pub ptr: I::AtomicInner,
    pub alloc: A,
}

impl<A: Allocator<usize>> Default for NodePtr<usize, A> {
    fn default() -> Self {
        Self {
            ptr: Default::default(),
            alloc: Default::default(),
        }
    }
}

// Safety: NodeRef only holds a pointer to the allocator. The allocator is Send + Sync,
// nodes are immutable after insertion, and ref_cnt updates are atomic.
unsafe impl<I: Idx, A: Allocator<I>> Send for NodePtr<I, A> {}
unsafe impl<I: Idx, A: Allocator<I>> Sync for NodePtr<I, A> {}

impl<A: Allocator<usize>> NodePtr<usize, A> {
    #[inline]
    pub fn from(idx: usize, alloc: A) -> Self {
        NodePtr {
            ptr: AtomicUsize::new(idx),
            alloc,
        }
    }
}

impl<A: Allocator<usize>> Debug for NodePtr<usize, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let node = self.alloc.index(self.ptr.load(Ordering::Relaxed));
        f.debug_struct("NodeRef")
            .field("idx", &self.ptr)
            .field("level", &node.level)
            .field("low", &node.low)
            .field("high", &node.high)
            .finish()
    }
}

impl<A: Allocator<usize>> Hash for NodePtr<usize, A> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let node = self.alloc.index(self.ptr.load(Ordering::Relaxed));
        node.level.hash(state);
        node.low.hash(state);
        node.high.hash(state);
    }
}

impl<A: Allocator<usize>> PartialEq for NodePtr<usize, A> {
    fn eq(&self, other: &Self) -> bool {
        let node1 = self.alloc.index(self.ptr.load(Ordering::Relaxed));
        let node2 = other.alloc.index(other.ptr.load(Ordering::Relaxed));
        node1.level == node2.level && node1.low == node2.low && node1.high == node2.high
    }
}

impl<A: Allocator<usize>> Eq for NodePtr<usize, A> {}
