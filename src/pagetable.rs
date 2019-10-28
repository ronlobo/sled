//! A simple wait-free, grow-only pagetable, assumes a dense keyspace.
#![allow(unsafe_code)]

use std::{
    alloc::{alloc_zeroed, Layout},
    convert::TryFrom,
    mem::{align_of, size_of},
    ops::{Deref, DerefMut},
    sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst},
};

use crossbeam_epoch::{pin, Atomic, Guard, Owned, Shared};

use crate::{debug_delay, pagecache::Page};

#[allow(unused)]
#[doc(hidden)]
pub const PAGETABLE_NODE_SZ: usize = size_of::<Node1<()>>();

const FAN_FACTOR: usize = 18;
const FAN_OUT: usize = 1 << FAN_FACTOR;
const FAN_MASK: usize = FAN_OUT - 1;

pub type PageId = u64;

pub struct PageView<'g> {
    pub(crate) read: Shared<'g, Page>,
    pub(crate) entry: &'g Atomic<Page>,
}

impl<'g> PageView<'g> {
    fn rcu<'b, F, B>(
        &self,
        f: F,
        guard: &'b Guard,
    ) -> Result<B, Shared<'b, Page>>
    where
        F: FnMut(&mut Page) -> B,
    {
        let mut old_pointer = self.read;
        loop {
            let mut clone: Owned<Page> = Owned::new(self.deref().clone());
            let b = f(clone.deref_mut());

            let result =
                self.entry.compare_and_set(self.read, clone, SeqCst, guard);

            match result {
                Ok(_) => return Ok(b),
                Err(cas_error)
                    if cas_error.current.version() == self.version() =>
                {
                    // we got here because the page was moved to a new
                    // location.
                    old_pointer = cas_error.current;
                    continue;
                }
                Err(cas_error) => {
                    return Err(cas_error.current);
                }
            }
        }
    }

    pub(crate) fn as_meta(&self) -> &crate::Meta {
        self.update.as_ref().unwrap().as_meta()
    }

    pub(crate) fn as_counter(&self) -> u64 {
        self.update.as_ref().unwrap().as_counter()
    }
}

impl<'g> Deref for PageView<'g> {
    type Target = Page;

    fn deref(&self) -> &Page {
        unsafe { self.read.deref() }
    }
}

struct Node1 {
    children: [Atomic<Node2>; FAN_OUT],
}

struct Node2 {
    children: [Atomic<Page>; FAN_OUT],
}

impl Node1 {
    fn new() -> Owned<Self> {
        let size = size_of::<Self>();
        let align = align_of::<Self>();

        unsafe {
            let layout = Layout::from_size_align_unchecked(size, align);

            #[allow(clippy::cast_ptr_alignment)]
            let ptr = alloc_zeroed(layout) as *mut Self;

            Owned::from_raw(ptr)
        }
    }
}

impl Node2 {
    fn new() -> Owned<Node2> {
        let size = size_of::<Self>();
        let align = align_of::<Self>();

        unsafe {
            let layout = Layout::from_size_align_unchecked(size, align);

            #[allow(clippy::cast_ptr_alignment)]
            let ptr = alloc_zeroed(layout) as *mut Self;

            Owned::from_raw(ptr)
        }
    }
}

impl Drop for Node1 {
    fn drop(&mut self) {
        drop_iter(self.children.iter());
    }
}

impl Drop for Node2 {
    fn drop(&mut self) {
        drop_iter(self.children.iter());
    }
}

fn drop_iter<T>(iter: core::slice::Iter<'_, Atomic<T>>) {
    let guard = pin();
    for child in iter {
        let shared_child = child.load(Relaxed, &guard);
        if shared_child.is_null() {
            // this does not leak because the PageTable is
            // assumed to be dense.
            break;
        }
        unsafe {
            drop(shared_child.into_owned());
        }
    }
}

/// A simple lock-free radix tree.
pub struct PageTable {
    head: Atomic<Node1>,
}

impl Default for PageTable {
    fn default() -> Self {
        let head = Node1::new();
        Self { head: Atomic::from(head) }
    }
}

impl PageTable {
    /// # Panics
    ///
    /// will panic if the item is not null already,
    /// which represents a serious failure to
    /// properly handle lifecycles of pages in the
    /// using system.
    pub fn insert(&self, pid: PageId, item: Page, guard: &Guard) {
        debug_delay();
        let tip = self.traverse(pid, guard);

        let old = tip.swap(Owned::new(item), Release, guard);
        assert!(old.is_null());
    }

    /// Try to get a value from the tree.
    pub fn get<'g>(
        &self,
        pid: PageId,
        guard: &'g Guard,
    ) -> Option<PageView<'g>> {
        debug_delay();
        let tip = self.traverse(pid, guard);

        let res = tip.load(Acquire, guard);
        if res.is_null() {
            None
        } else {
            let page_view = PageView { read: res, entry: tip };

            Some(page_view)
        }
    }

    fn traverse<'g>(self, k: PageId, guard: &'g Guard) -> &'g Atomic<Page> {
        let (l1k, l2k) = split_fanout(k);

        debug_delay();
        let head = self.head.load(Acquire, guard);

        debug_delay();
        let l1 = unsafe { head.deref().children };

        debug_delay();
        let mut l2_ptr = l1[l1k].load(Acquire, guard);

        if l2_ptr.is_null() {
            let next_child = Node2::new();

            debug_delay();
            let ret = l1[l1k].compare_and_set(
                Shared::null(),
                next_child,
                Release,
                guard,
            );

            l2_ptr = match ret {
                Ok(next_child) => next_child,
                Err(returned) => {
                    drop(returned.new);
                    returned.current
                }
            };
        }

        debug_delay();
        let l2 = unsafe { l2_ptr.deref().children };

        &l2[l2k]
    }
}

#[inline]
fn split_fanout(id: PageId) -> (usize, usize) {
    // right shift 32 on 32-bit pointer systems panics
    #[cfg(target_pointer_width = "64")]
    assert!(
        id <= 1 << (FAN_FACTOR * 2),
        "trying to access key of {}, which is \
         higher than 2 ^ {}",
        id,
        (FAN_FACTOR * 2)
    );

    let left = id >> FAN_FACTOR;
    let right = id & u64::try_from(FAN_MASK).unwrap();

    (safe_usize(left), safe_usize(right))
}

#[inline]
fn safe_usize(value: PageId) -> usize {
    usize::try_from(value).unwrap()
}

impl Drop for PageTable {
    fn drop(&mut self) {
        let guard = pin();
        let head = self.head.load(Relaxed, &guard);
        unsafe {
            drop(head.into_owned());
        }
    }
}

#[test]
fn test_split_fanout() {
    assert_eq!(
        split_fanout(0b11_1111_1111_1111_1111),
        (0, 0b11_1111_1111_1111_1111)
    );
    assert_eq!(
        split_fanout(0b111_1111_1111_1111_1111),
        (0b1, 0b11_1111_1111_1111_1111)
    );
}
