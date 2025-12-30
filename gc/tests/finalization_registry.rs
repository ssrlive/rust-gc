use gc::{Finalize, Gc, Trace, force_collect};
use std::cell::Cell;
use std::thread::LocalKey;

// A small callback-like value whose finalizer flips a flag.
struct Callback(&'static LocalKey<Cell<bool>>);

impl Finalize for Callback {
    fn finalize(&self) {
        self.0.with(|c| c.set(true));
    }
}

unsafe impl Trace for Callback {
    #[inline]
    unsafe fn trace(&self) {}

    #[inline]
    unsafe fn is_marked_ephemeron(&self) -> bool {
        false
    }

    #[inline]
    unsafe fn weak_trace(&self, _ephemeron_queue: &mut Vec<(gc::GcPointer, gc::GcPointer)>) {}

    #[inline]
    unsafe fn root(&self) {}

    #[inline]
    unsafe fn unroot(&self) {}

    #[inline]
    fn finalize_glue(&self) {
        Finalize::finalize(self);
    }
}

#[test]
fn finalization_registry_invokes_callback_when_key_collected() {
    thread_local!(static CALLED: Cell<bool> = const { Cell::new(false) });

    // Create a key and register a WeakPair (key -> Callback) in the registry.
    let mut registry: Vec<gc::WeakPair<_, _>> = Vec::new();
    {
        let key = Gc::new(42usize);
        let cb = Callback(&CALLED);
        // store the WeakPair in the registry; the registry lives beyond the scope
        registry.push(key.create_weak_pair(cb));
        // drop the strong key reference so it can be collected
    }

    // Trigger GC; after collection the callback's finalizer should run
    force_collect();

    CALLED.with(|c| assert!(c.get(), "finalizer was not invoked"));
}
