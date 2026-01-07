use std::cmp::Ordering as CmpOrd;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::{
    alloc::Allocator,
    node::{Idx, Node, NodePtr},
};

#[allow(dead_code)]
pub trait Set<I: Idx, A: Allocator<I>> {
    fn get_or_insert(&self, hash: u64, value: NodePtr<I, A>) -> (I, bool);
}

pub struct LockFreeSet<I: Idx, A: Allocator<I>> {
    buckets: Box<[I::AtomicInner]>,
    size_exp: usize,
    alloc: A,
}

impl<A: Allocator<usize>> LockFreeSet<usize, A> {
    pub fn with_capacity(cap: usize) -> Self {
        let size_exp = cap.isolate_highest_one().trailing_zeros() + 1;
        let buckets = (0..(1 << size_exp))
            .map(|_| AtomicUsize::new(usize::NULL))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        LockFreeSet {
            buckets,
            size_exp: size_exp as usize,
            alloc: A::default(),
        }
    }
}

impl<A: Allocator<usize>> Set<usize, A> for LockFreeSet<usize, A> {
    fn get_or_insert(&self, hash: u64, new_ptr: NodePtr<usize, A>) -> (usize, bool) {
        let idx = hash & ((1 << self.size_exp) - 1);
        let head = &self.buckets[idx as usize];

        // 只读一次新节点的 key（要求调用者不会并发修改这些字段）
        let new_key = self.alloc.index(new_ptr.ptr.load(Ordering::Relaxed)).key();
        let new_ptr = new_ptr.ptr.load(Ordering::Relaxed);

        'retry: loop {
            // prev 指向“指向当前节点的那个 AtomicPtr”（可能是 head，也可能是某个节点的 next）
            let mut prev: &AtomicUsize = head;
            let mut curr: *mut Node<usize, A> = prev.load(Ordering::Acquire) as _;

            while !curr.is_null() {
                // 由于没有删除，curr 指向的 Node 永远有效
                let curr_ref = unsafe { &*curr };
                let curr_key = curr_ref.key();

                match curr_key.cmp(&new_key) {
                    CmpOrd::Equal => {
                        // 找到相同值：不插入，返回已存在节点
                        return (curr as usize, false);
                    }
                    CmpOrd::Greater => {
                        // 应该插入在 curr 之前（也就是 prev -> curr 之间）
                        break;
                    }
                    CmpOrd::Less => {
                        // 继续向后走
                        prev = &curr_ref.next.ptr;
                        curr = prev.load(Ordering::Acquire) as _;
                    }
                }
            }

            // 走到这里表示：没看到相同 key，且插入位置是 prev -> curr 之间
            // 先把新节点的 next 指向 curr（此时新节点还没发布到链表里）
            self.alloc
                .index(new_ptr)
                .next
                .ptr
                .store(curr as usize, Ordering::Relaxed);

            // CAS：如果 prev 仍然指向 curr，则把 prev 改为指向 ptr_new
            match prev.compare_exchange(
                curr as usize,
                new_ptr,
                Ordering::AcqRel, // 成功：发布新节点（Release），并同步读到的链表状态（Acquire）
                Ordering::Acquire, // 失败：获取最新的 prev 值
            ) {
                Ok(_) => {
                    // 插入成功
                    return (new_ptr, true);
                }
                Err(_actual_now) => {
                    // 并发有变化（有人插入了节点），重试：
                    // 关键点：重试会重新遍历并检查 Equal，从而避免并发下插入重复 key。
                    continue 'retry;
                }
            }
        }
    }
}
