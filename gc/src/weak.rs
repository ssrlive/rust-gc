use std::ptr::NonNull;

use crate::Gc;
use crate::gc::{GcBox, GcBoxType};
use crate::trace::{Finalize, Trace};

/// A weak reference to a `Gc<T>`.
///
/// A `WeakGc` does not prevent the value stored in the garbage collector from
/// being collected, and it may become invalid if the value is collected.
///
/// `WeakGc` is `Clone` and `Copy`, and it is safe to have multiple `WeakGc`
/// pointing to the same value.
#[derive(Clone, Copy)]
pub struct WeakGc<T: Trace + ?Sized + 'static> {
    ptr: NonNull<GcBox<T>>,
}

impl<T: Trace> WeakGc<T> {
    /// Creates a new `WeakGc` from a value.
    ///
    /// This creates a `Gc` first, then returns a weak reference to it.
    pub fn new(value: T) -> Self {
        let gc = Gc::new(value);
        gc.clone_weak_gc()
    }

    /// Returns the value if it is still alive, or `None` if it has been collected.
    pub fn value(&self) -> Option<&T> {
        if unsafe { self.ptr.as_ref().header.is_alive() } {
            Some(unsafe { self.ptr.as_ref().value() })
        } else {
            None
        }
    }
}

impl<T: Trace + ?Sized> WeakGc<T> {
    /// Creates a new `WeakGc` from an existing `Gc<T>`.
    ///
    /// This does not increase the root count of the `Gc`.
    pub fn from_gc(gc: &Gc<T>) -> Self {
        // Ensure we clear the root bit when storing the pointer so
        // dereferencing the weak pointer works correctly.
        WeakGc {
            ptr: unsafe { crate::clear_root_bit(gc.ptr_root.get()) },
        }
    }

    /// Creates a new `WeakGc` from a `GcBox` pointer.
    ///
    /// This is unsafe because it assumes the pointer is valid.
    ///
    /// # Safety
    /// `ptr` must point to a valid `GcBox<T>` that is present on the
    /// current thread's GC chain. The caller must ensure the pointer is
    /// non-dangling for the duration of use.
    pub unsafe fn from_gc_box(ptr: NonNull<GcBox<T>>) -> Self {
        WeakGc {
            ptr: unsafe { crate::clear_root_bit(ptr) },
        }
    }
}

/// A weak pair containing a key and a value.
///
/// The key is a `WeakGc`, and the value is stored in a `GcBox` with type `Ephemeron`.
/// When the key is collected, the value can also be collected.
pub struct WeakPair<K: Trace + 'static, V: Trace + 'static> {
    key: WeakGc<K>,
    value: NonNull<GcBox<V>>,
}

impl<K: Trace + 'static, V: Trace + 'static> WeakPair<K, V> {
    /// Creates a new `WeakPair` from a key `Gc` and a value.
    ///
    /// The value is stored in an ephemeron box.
    pub fn from_gc_value_pair(key_gc: NonNull<GcBox<K>>, value: V) -> Self {
        let value_ptr = GcBox::new(value, GcBoxType::Ephemeron);
        WeakPair {
            key: unsafe { WeakGc::from_gc_box(key_gc) },
            value: value_ptr,
        }
    }

    /// Returns the key if it is still alive.
    pub fn key(&self) -> Option<&K> {
        self.key.value()
    }

    /// Returns the value if the key is still alive.
    pub fn value(&self) -> Option<&V> {
        if self.key.value().is_some() {
            unsafe { Some(self.value.as_ref().value()) }
        } else {
            None
        }
    }
}

impl<K: Trace + 'static, V: Trace + 'static> Finalize for WeakPair<K, V> {}

unsafe impl<K: Trace + 'static, V: Trace + 'static> Trace for WeakPair<K, V> {
    #[inline]
    unsafe fn trace(&self) {}

    #[inline]
    unsafe fn is_marked_ephemeron(&self) -> bool {
        false
    }

    #[inline]
    unsafe fn weak_trace(&self, ephemeron_queue: &mut Vec<(crate::GcPointer, crate::GcPointer)>) {
        // Coerce concrete GcBox pointers to trait-object GcBox pointers
        // by coercing a reference; this relies on the nightly Unsize coercion.
        let key_dyn_ref: &GcBox<dyn Trace> = unsafe { &*self.key.ptr.as_ptr() };
        let value_dyn_ref: &GcBox<dyn Trace> = unsafe { &*self.value.as_ptr() };
        let key_ptr = NonNull::from(key_dyn_ref);
        let value_ptr = NonNull::from(value_dyn_ref);
        ephemeron_queue.push((key_ptr, value_ptr));
    }

    #[inline]
    unsafe fn root(&self) {}

    #[inline]
    unsafe fn unroot(&self) {}

    #[inline]
    fn finalize_glue(&self) {}
}
