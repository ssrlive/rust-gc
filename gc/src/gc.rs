use crate::trace::Trace;
use crate::{GcPointer, set_data_ptr};
use std::alloc::{Layout, alloc, dealloc};
use std::cell::{Cell, RefCell};
use std::mem;
use std::ptr::{self, NonNull};

#[cfg(feature = "nightly")]
use std::marker::Unsize;

pub enum GcBoxType {
    Standard,
    Ephemeron,
}

struct GcState {
    stats: GcStats,
    config: GcConfig,
    boxes_start: Option<GcPointer>,
}

impl Drop for GcState {
    fn drop(&mut self) {
        if !self.config.leak_on_drop {
            collect_garbage(self);
        }
        // We have no choice but to leak any remaining nodes that
        // might be referenced from other thread-local variables.
    }
}

// Whether or not the thread is currently in the sweep phase of garbage collection.
// During this phase, attempts to dereference a `Gc<T>` pointer will trigger a panic.
thread_local!(pub static GC_DROPPING: Cell<bool> = const { Cell::new(false) });
struct DropGuard;
impl DropGuard {
    fn new() -> DropGuard {
        GC_DROPPING.with(|dropping| dropping.set(true));
        DropGuard
    }
}
impl Drop for DropGuard {
    fn drop(&mut self) {
        GC_DROPPING.with(|dropping| dropping.set(false));
    }
}
#[must_use]
pub fn finalizer_safe() -> bool {
    GC_DROPPING.with(|dropping| !dropping.get())
}

// The garbage collector's internal state.
thread_local!(static GC_STATE: RefCell<GcState> = RefCell::new(GcState {
    stats: GcStats::default(),
    config: GcConfig::default(),
    boxes_start: None,
}));

const MARK_MASK: usize = 1 << (usize::BITS - 1);
const ROOTS_MASK: usize = !MARK_MASK;
const ROOTS_MAX: usize = ROOTS_MASK; // max allowed value of roots

pub(crate) struct GcBoxHeader {
    roots: Cell<usize>, // high bit is used as mark flag
    next: Cell<Option<GcPointer>>,
}

impl GcBoxHeader {
    #[inline]
    pub fn new() -> Self {
        GcBoxHeader {
            roots: Cell::new(1), // unmarked and roots count = 1
            next: Cell::new(None),
        }
    }

    #[inline]
    pub fn new_ephemeron(next: Option<GcPointer>) -> Self {
        GcBoxHeader {
            roots: Cell::new(0),
            next: Cell::new(next),
        }
    }

    #[inline]
    pub fn roots(&self) -> usize {
        self.roots.get() & ROOTS_MASK
    }

    #[inline]
    pub fn inc_roots(&self) {
        let roots = self.roots.get();

        // abort if the count overflows to prevent `mem::forget` loops
        // that could otherwise lead to erroneous drops
        if (roots & ROOTS_MASK) < ROOTS_MAX {
            self.roots.set(roots + 1); // we checked that this wont affect the high bit
        } else {
            panic!("roots counter overflow");
        }
    }

    #[inline]
    pub fn dec_roots(&self) {
        self.roots.set(self.roots.get() - 1); // no underflow check
    }

    #[inline]
    pub fn is_marked(&self) -> bool {
        self.roots.get() & MARK_MASK != 0
    }

    #[inline]
    pub fn mark(&self) {
        self.roots.set(self.roots.get() | MARK_MASK);
    }

    #[inline]
    pub fn unmark(&self) {
        self.roots.set(self.roots.get() & !MARK_MASK);
    }

    #[inline]
    pub fn is_alive(&self) -> bool {
        // A box is considered alive if it is currently present in the
        // thread-local GC chain. We scan the chain for the header's
        // address. This is cheaper and more robust than relying on
        // mark-bits which are transient during collection.
        use std::ptr;
        GC_STATE.with(|st| {
            let st = st.borrow();
            let mut cur = st.boxes_start;
            while let Some(node) = cur {
                let header_ptr = unsafe { &node.as_ref().header as *const GcBoxHeader };
                if ptr::eq(self as *const GcBoxHeader, header_ptr) {
                    return true;
                }
                cur = unsafe { node.as_ref().header.next.get() };
            }
            false
        })
    }
}

#[repr(C)] // to justify the layout computations in GcBox::from_box, Gc::from_raw
pub struct GcBox<T: Trace + ?Sized + 'static> {
    pub(crate) header: GcBoxHeader,
    data: T,
}

impl<T: Trace> GcBox<T> {
    /// Allocates a garbage collected `GcBox` on the heap,
    /// and appends it to the thread-local `GcBox` chain. This might
    /// trigger a collection.
    ///
    /// A `GcBox` allocated this way starts its life rooted.
    pub(crate) fn new(data: T, box_type: GcBoxType) -> NonNull<Self> {
        let header = match box_type {
            GcBoxType::Standard => GcBoxHeader::new(),
            GcBoxType::Ephemeron => GcBoxHeader::new_ephemeron(None),
        };
        let gcbox = NonNull::from(Box::leak(Box::new(GcBox { header, data })));
        unsafe { insert_gcbox(gcbox) };
        gcbox
    }
}

impl<
    #[cfg(not(feature = "nightly"))] T: Trace,
    #[cfg(feature = "nightly")] T: Trace + Unsize<dyn Trace> + ?Sized,
> GcBox<T>
{
    /// Consumes a `Box`, moving the value inside into a new `GcBox`
    /// on the heap. Adds the new `GcBox` to the thread-local `GcBox`
    /// chain. This might trigger a collection.
    ///
    /// A `GcBox` allocated this way starts its life rooted.
    pub(crate) fn from_box(value: Box<T>) -> NonNull<Self> {
        let header_layout = Layout::new::<GcBoxHeader>();
        let value_layout = Layout::for_value::<T>(&*value);
        // This relies on GcBox being #[repr(C)].
        let gcbox_layout = header_layout.extend(value_layout).unwrap().0.pad_to_align();

        unsafe {
            // Allocate the GcBox in a way that's compatible with Box,
            // since the collector will deallocate it via
            // Box::from_raw.
            let gcbox_addr = alloc(gcbox_layout);

            // Since we're not allowed to move the value out of an
            // active Box, and we will need to deallocate the Box
            // without calling the destructor, convert it to a raw
            // pointer first.
            let value = Box::into_raw(value);

            // Create a pointer with the metadata of value and the
            // address and provenance of the GcBox.
            let gcbox = set_data_ptr(value as *mut GcBox<T>, gcbox_addr);

            // Move the data.
            ptr::addr_of_mut!((*gcbox).header).write(GcBoxHeader::new());
            ptr::addr_of_mut!((*gcbox).data)
                .cast::<u8>()
                .copy_from_nonoverlapping(value.cast::<u8>(), value_layout.size());

            // Deallocate the former Box. (Box only allocates for size
            // != 0.)
            if value_layout.size() != 0 {
                dealloc(value.cast::<u8>(), value_layout);
            }

            // Add the new GcBox to the chain and return it.
            let gcbox = NonNull::new_unchecked(gcbox);
            insert_gcbox(gcbox);
            gcbox
        }
    }
}

/// Add a new `GcBox` to the current thread's `GcBox` chain. This
/// might trigger a collection first if enough bytes have been
/// allocated since the previous collection.
///
/// # Safety
///
/// `gcbox` must point to a valid `GcBox` that is not yet in a `GcBox`
/// chain.
unsafe fn insert_gcbox(gcbox: GcPointer) {
    GC_STATE.with(|st| {
        let mut st = st.borrow_mut();

        // XXX We should probably be more clever about collecting
        if st.stats.bytes_allocated > st.config.threshold {
            collect_garbage(&mut st);

            if st.stats.bytes_allocated as f64
                > st.config.threshold as f64 * st.config.used_space_ratio
            {
                // we didn't collect enough, so increase the
                // threshold for next time, to avoid thrashing the
                // collector too much/behaving quadratically.
                st.config.threshold =
                    (st.stats.bytes_allocated as f64 / st.config.used_space_ratio) as usize;
            }
        }

        let next = st.boxes_start.replace(gcbox);
        unsafe { gcbox.as_ref().header.next.set(next) };

        // We allocated some bytes! Let's record it
        st.stats.bytes_allocated += mem::size_of_val::<GcBox<_>>(unsafe { gcbox.as_ref() });
    });
}

impl<T: Trace + ?Sized> GcBox<T> {
    /// Returns `true` if the two references refer to the same `GcBox`.
    pub(crate) fn ptr_eq(this: &GcBox<T>, other: &GcBox<T>) -> bool {
        // Use .header to ignore fat pointer vtables, to work around
        // https://github.com/rust-lang/rust/issues/46139
        ptr::eq(&raw const this.header, &raw const other.header)
    }
}

impl<T: Trace + ?Sized> GcBox<T> {
    /// Marks this `GcBox` and marks through its data.
    pub(crate) unsafe fn trace_inner(&self) {
        if !self.header.is_marked() {
            self.header.mark();
            unsafe { self.data.trace() };
        }
    }

    /// Trace inner data for weak/ephemeron relationships.
    pub(crate) unsafe fn weak_trace_inner(&self, queue: &mut Vec<(GcPointer, GcPointer)>) {
        unsafe { self.data.weak_trace(queue) };
    }
}

impl<T: Trace + ?Sized> GcBox<T> {
    /// Increases the root count on this `GcBox`.
    /// Roots prevent the `GcBox` from being destroyed by the garbage collector.
    pub(crate) unsafe fn root_inner(&self) {
        self.header.inc_roots();
    }

    /// Decreases the root count on this `GcBox`.
    /// Roots prevent the `GcBox` from being destroyed by the garbage collector.
    pub(crate) unsafe fn unroot_inner(&self) {
        self.header.dec_roots();
    }

    /// Returns a pointer to the `GcBox`'s value, without dereferencing it.
    pub(crate) fn value_ptr(this: *const GcBox<T>) -> *const T {
        unsafe { ptr::addr_of!((*this).data) }
    }

    /// Returns a reference to the `GcBox`'s value.
    pub(crate) fn value(&self) -> &T {
        &self.data
    }
}

/// Collects garbage.
fn collect_garbage(st: &mut GcState) {
    struct Unmarked<'a> {
        incoming: &'a Cell<Option<GcPointer>>,
        this: GcPointer,
    }

    unsafe fn sweep(finalized: Vec<Unmarked<'_>>, bytes_allocated: &mut usize) {
        let _guard = DropGuard::new();
        for node in finalized.into_iter().rev() {
            if unsafe { node.this.as_ref().header.is_marked() } {
                continue;
            }
            let incoming = node.incoming;
            let node = unsafe { Box::from_raw(node.this.as_ptr()) };
            *bytes_allocated -= mem::size_of_val::<GcBox<_>>(&*node);
            incoming.set(node.header.next.take());
        }
    }

    st.stats.collections_performed += 1;

    let head = Cell::from_mut(&mut st.boxes_start);

    type EPair = (GcPointer, GcPointer);

    unsafe fn initial_mark_and_collect_ephemerons(
        head: &Cell<Option<GcPointer>>,
        ephemeron_queue: &mut Vec<EPair>,
    ) {
        let mut mark_head = head.get();
        while let Some(node) = mark_head {
            if unsafe { node.as_ref().header.roots() } > 0 {
                unsafe { node.as_ref().trace_inner() };
                unsafe { node.as_ref().weak_trace_inner(ephemeron_queue) };
            }
            mark_head = unsafe { node.as_ref().header.next.get() };
        }
    }

    unsafe fn process_ephemeron_queue(ephemeron_queue: &mut Vec<EPair>) {
        let mut idx = 0usize;
        while idx < ephemeron_queue.len() {
            let (key_ptr, value_ptr) = ephemeron_queue[idx];
            idx += 1;
            let key_marked = unsafe { key_ptr.as_ref().header.is_marked() }
                || unsafe { key_ptr.as_ref().value().is_marked_ephemeron() };
            if key_marked && !unsafe { value_ptr.as_ref().header.is_marked() } {
                unsafe { value_ptr.as_ref().trace_inner() };
                unsafe { value_ptr.as_ref().weak_trace_inner(ephemeron_queue) };
            }
        }
    }

    unsafe fn collect_unmarked_nodes<'a>(head: &'a Cell<Option<GcPointer>>) -> Vec<Unmarked<'a>> {
        let mut unmarked = Vec::new();
        let mut unmark_head = head;
        while let Some(node) = unmark_head.get() {
            if unsafe { node.as_ref().header.is_marked() } {
                unsafe { node.as_ref().header.unmark() };
            } else {
                unmarked.push(Unmarked {
                    incoming: unmark_head,
                    this: node,
                });
            }
            unmark_head = unsafe { &node.as_ref().header.next };
        }
        unmarked
    }

    unsafe {
        let mut ephemeron_queue: Vec<EPair> = Vec::new();

        initial_mark_and_collect_ephemerons(head, &mut ephemeron_queue);
        process_ephemeron_queue(&mut ephemeron_queue);

        let unmarked = collect_unmarked_nodes(head);

        if unmarked.is_empty() {
            return;
        }

        for node in &unmarked {
            Trace::finalize_glue(&node.this.as_ref().data);
        }

        let mut ephemeron_queue2: Vec<EPair> = Vec::new();
        initial_mark_and_collect_ephemerons(head, &mut ephemeron_queue2);
        process_ephemeron_queue(&mut ephemeron_queue2);

        sweep(unmarked, &mut st.stats.bytes_allocated);
    }
}

/// Immediately triggers a garbage collection on the current thread.
///
/// This will panic if executed while a collection is currently in progress
pub fn force_collect() {
    GC_STATE.with(|st| {
        let mut st = st.borrow_mut();
        collect_garbage(&mut st);
    });
}

pub struct GcConfig {
    pub threshold: usize,
    /// after collection we want the the ratio of used/total to be no
    /// greater than this (the threshold grows exponentially, to avoid
    /// quadratic behavior when the heap is growing linearly with the
    /// number of `new` calls):
    pub used_space_ratio: f64,
    /// For short-running processes it is not always appropriate to run
    /// GC, sometimes it is better to let system free the resources
    pub leak_on_drop: bool,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            used_space_ratio: 0.7,
            threshold: 100,
            leak_on_drop: false,
        }
    }
}

#[cfg(feature = "unstable-config")]
pub fn configure(configurer: impl FnOnce(&mut GcConfig)) {
    GC_STATE.with(|st| {
        let mut st = st.borrow_mut();
        configurer(&mut st.config);
    });
}

#[derive(Clone, Default)]
pub struct GcStats {
    pub bytes_allocated: usize,
    pub collections_performed: usize,
}

#[allow(dead_code)]
#[must_use]
pub fn stats() -> GcStats {
    GC_STATE.with(|st| st.borrow().stats.clone())
}
