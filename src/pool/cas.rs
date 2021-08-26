//! Stack based on CAS atomics
//!
//! To reduce the chance of hitting the ABA problem we use a 32-bit offset + a 32-bit version tag
//! instead of a 64-bit pointer. The version tag will be bumped on each successful `pop` operation.

use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    num::{NonZeroU32, NonZeroU64},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

/// Unfortunate implementation detail required to use the
/// [`Pool.grow_exact`](struct.Pool.html#method.grow_exact) method
pub struct Node<T> {
    next: Atomic<Node<T>>,
    pub(crate) data: UnsafeCell<T>,
}

impl<T> Node<T> {
    fn next(&self) -> &Atomic<Node<T>> {
        &self.next
    }
}

pub struct Stack<T> {
    head: Atomic<Node<T>>,
}

impl<T> Stack<T> {
    pub const fn new() -> Self {
        Self {
            head: Atomic::null(),
        }
    }

    pub fn push(&self, new_head: Ptr<Node<T>>) {
        let mut head = self.head.load(Ordering::Relaxed);

        loop {
            unsafe {
                new_head
                    .as_raw()
                    .as_ref()
                    .next()
                    .store(head, Ordering::Relaxed);
            }

            if let Err(p) = self.head.compare_and_exchange_weak(
                head,
                Some(new_head),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                head = p;
            } else {
                return;
            }
        }
    }

    pub fn try_pop(&self) -> Option<Ptr<Node<T>>> {
        loop {
            if let Some(mut head) = self.head.load(Ordering::Acquire) {
                let next = unsafe { head.as_raw().as_ref().next().load(Ordering::Relaxed) };

                if self
                    .head
                    .compare_and_exchange_weak(
                        Some(head),
                        next,
                        Ordering::Release,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    head.incr_tag();
                    return Some(head);
                }
            } else {
                // stack observed empty
                return None;
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn anchor<T>() -> *mut T {
    static mut ANCHOR: u8 = 0;
    (unsafe { &mut ANCHOR } as *mut u8 as usize & !(core::mem::align_of::<T>() - 1)) as *mut T
}

/// On x86_64, anchored pointer. This is a (signed) 32-bit offset from `anchor` plus a 32-bit tag
/// On x86, this is a pointer plus a 32-bit tag
pub struct Ptr<T> {
    inner: NonZeroU64,
    _marker: PhantomData<*mut T>,
}

impl<T> Clone for Ptr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Ptr<T> {}

fn initial_tag_value() -> NonZeroU32 {
    NonZeroU32::new(1).unwrap()
}

impl<T> Ptr<T> {
    #[cfg(target_arch = "x86_64")]
    pub fn new(p: *mut T) -> Option<Self> {
        use core::convert::TryFrom;

        i32::try_from((p as isize).wrapping_sub(anchor::<T>() as isize))
            .ok()
            .map(|offset| unsafe { Ptr::from_parts(initial_tag_value(), offset) })
    }

    #[cfg(target_arch = "x86")]
    pub fn new(p: *mut T) -> Option<Self> {
        Some(unsafe { Ptr::from_parts(initial_tag_value(), p as i32) })
    }

    unsafe fn from_parts(tag: NonZeroU32, offset: i32) -> Self {
        Self {
            inner: NonZeroU64::new_unchecked((tag.get() as u64) << 32 | (offset as u32 as u64)),
            _marker: PhantomData,
        }
    }

    fn from_u64(p: u64) -> Option<Self> {
        NonZeroU64::new(p).map(|inner| Self {
            inner,
            _marker: PhantomData,
        })
    }

    fn into_u64(&self) -> u64 {
        self.inner.get()
    }

    fn tag(&self) -> NonZeroU32 {
        let tag = (self.inner.get() >> 32) as u32;
        debug_assert_ne!(0, tag, "broken non-zero invariant");
        unsafe { NonZeroU32::new_unchecked(tag) }
    }

    fn incr_tag(&mut self) {
        let maybe_zero_tag = self.tag().get().wrapping_add(1);
        let tag = NonZeroU32::new(maybe_zero_tag).unwrap_or(initial_tag_value());
        let offset = self.offset();

        *self = unsafe { Ptr::from_parts(tag, offset) };
    }

    fn offset(&self) -> i32 {
        self.inner.get() as i32
    }

    #[cfg(target_arch = "x86_64")]
    fn as_raw(&self) -> NonNull<T> {
        unsafe {
            NonNull::new_unchecked(
                (anchor::<T>() as *mut u8).offset(self.offset() as isize) as *mut T
            )
        }
    }

    #[cfg(target_arch = "x86")]
    fn as_raw(&self) -> NonNull<T> {
        unsafe { NonNull::new_unchecked(self.offset() as *mut T) }
    }

    pub fn dangling() -> Self {
        unsafe { Self::from_parts(initial_tag_value(), 1) }
    }

    pub unsafe fn as_ref(&self) -> &T {
        &*self.as_raw().as_ptr()
    }
}

struct Atomic<T> {
    inner: AtomicU64,
    _marker: PhantomData<*mut T>,
}

impl<T> Atomic<T> {
    const fn null() -> Self {
        Self {
            inner: AtomicU64::new(0),
            _marker: PhantomData,
        }
    }

    fn compare_and_exchange_weak(
        &self,
        current: Option<Ptr<T>>,
        new: Option<Ptr<T>>,
        succ: Ordering,
        fail: Ordering,
    ) -> Result<(), Option<Ptr<T>>> {
        self.inner
            .compare_exchange_weak(
                current.map(|p| p.into_u64()).unwrap_or(0),
                new.map(|p| p.into_u64()).unwrap_or(0),
                succ,
                fail,
            )
            .map(drop)
            .map_err(Ptr::from_u64)
    }

    fn load(&self, ord: Ordering) -> Option<Ptr<T>> {
        NonZeroU64::new(self.inner.load(ord)).map(|inner| Ptr {
            inner,
            _marker: PhantomData,
        })
    }

    fn store(&self, val: Option<Ptr<T>>, ord: Ordering) {
        self.inner
            .store(val.map(|p| p.into_u64()).unwrap_or(0), ord)
    }
}