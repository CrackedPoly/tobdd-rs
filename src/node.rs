use std::sync::atomic::{AtomicPtr, AtomicUsize};

use crate::hash;

#[derive(Default)]
pub struct Node {
    pub ref_cnt: AtomicUsize,
    pub level: usize,
    pub low: *mut Node,
    pub high: *mut Node,
    pub next: AtomicPtr<Node>,
    pub hash: u64,
}

unsafe impl Send for Node {}
unsafe impl Sync for Node {}

impl Node {
    #[inline]
    pub fn key(&self) -> (usize, *mut Node, *mut Node) {
        (self.level, self.low, self.high)
    }

    #[inline]
    pub fn new(level: usize) -> Self {
        Node {
            ref_cnt: AtomicUsize::new(0),
            level,
            low: std::ptr::null_mut(),
            high: std::ptr::null_mut(),
            next: Default::default(),
            hash: hash::splitmix64_3(level as u64, 0, 0),
        }
    }

    #[inline]
    pub fn from(level: usize, low: *mut Node, high: *mut Node) -> Self {
        Node {
            ref_cnt: AtomicUsize::new(0),
            level,
            low,
            high,
            next: Default::default(),
            hash: hash::splitmix64_3(level as u64, low as u64, high as u64),
        }
    }
}

#[allow(clippy::mut_from_ref)]
pub trait NodePtr {
    fn node_hash(&self) -> u64;
    fn rehash(&mut self);
    fn level(&mut self) -> &mut usize;
    fn low(&mut self) -> &mut *mut Node;
    fn high(&mut self) -> &mut *mut Node;
    fn ref_cnt(&self) -> &AtomicUsize;
    fn next(&self) -> &AtomicPtr<Node>;
}

impl NodePtr for *mut Node {
    #[inline]
    fn node_hash(&self) -> u64 {
        let node = unsafe { &**self };
        node.hash
    }

    #[inline]
    fn rehash(&mut self) {
        let node = unsafe { &mut **self };
        node.hash = hash::splitmix64_3(node.level as u64, node.low as u64, node.high as u64);
    }

    #[inline]
    fn level(&mut self) -> &mut usize {
        unsafe { &mut (**self).level }
    }

    #[inline]
    fn low(&mut self) -> &mut *mut Node {
        unsafe { &mut (**self).low }
    }

    #[inline]
    fn high(&mut self) -> &mut *mut Node {
        unsafe { &mut (**self).high }
    }

    #[inline]
    fn ref_cnt(&self) -> &AtomicUsize {
        unsafe { &(**self).ref_cnt }
    }

    #[inline]
    fn next(&self) -> &AtomicPtr<Node> {
        unsafe { &(**self).next }
    }
}
