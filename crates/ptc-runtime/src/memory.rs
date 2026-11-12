//! The bounds V8 offers only through raw pointers. This is the one module of the workspace
//! allowed to hold `unsafe` code (`archi/07-code-architecture.md`, "Dependency rules"): every
//! block carries a `// SAFETY:` comment, and the architecture tests refuse `unsafe` anywhere
//! else.
//!
//! [`capped_allocator`] is the allocator of an isolate's array buffers. V8 keeps their memory
//! outside its heap, so the heap limit does not bound it: without a cap, one line
//! (`new ArrayBuffer(2 ** 36)`) made V8 abort the process, and 1.5 GiB of typed arrays fitted
//! under a 64 MiB heap. The allocator refuses an allocation that would take the bytes it holds
//! past its budget. V8 then collects garbage and retries, so it fails only when the buffers the
//! script can still reach leave no room, and the script gets V8's own
//! `RangeError: Array buffer allocation failed`.
//!
//! V8 asks the near-heap-limit callback at the start of that last collection, whatever the
//! heap's size. [`ArrayBufferBudget::took_refusal`] tells the callback that such a call follows a
//! refused allocation, not a full heap: [`track_refusals`] forgets a refusal at the start of every
//! collection, and a refusal V8 makes after asking the callback ends the sequence.
//!
//! [`pin_stack`] sets the limit of V8's stack a fixed distance below the frame that calls into
//! JavaScript. V8 sets it once, [`SCRIPT_STACK`] below the top of the isolate's thread, so the
//! depth a script reached (and could catch, and observe) depended on every Rust frame between
//! the thread's start and the call: on the build. Pinned before each call, it depends on V8 and
//! on the few frames of rusty_v8 between the call and V8.

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks, unsafe_op_in_unsafe_fn)]

use std::alloc::{GlobalAlloc as _, Layout, System};
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use deno_core::v8;

/// The alignment of every buffer: what `malloc` gives, enough for any typed array.
const ALIGN: usize = 16;

/// The stack a call into JavaScript may use: V8's default `--stack-size` on 64-bit hosts.
pub(crate) const SCRIPT_STACK: usize = 984 * 1024;

/// The stack of an isolate's thread: the script's, the Rust frames above it, and room below the
/// limit for what V8 runs in C++ once JavaScript reached it.
pub(crate) const ISOLATE_THREAD_STACK: usize = 4 * 1024 * 1024;

/// No refused allocation waits for V8's retry.
const SETTLED: u8 = 0;
/// The allocator refused, and no collection started since: V8's next call to the near-heap-limit
/// callback, if any, is the one that opens its last-resort collection.
const REFUSED: u8 = 1;
/// The callback was told: the next refusal is V8's last attempt.
const TOLD: u8 = 2;

/// The bytes an isolate's array buffers may hold at once, and the bytes they hold.
#[derive(Debug)]
pub(crate) struct ArrayBufferBudget {
    held: AtomicUsize,
    cap: usize,
    refusal: AtomicU8,
}

impl ArrayBufferBudget {
    fn new(cap: usize) -> Self {
        Self {
            held: AtomicUsize::new(0),
            cap,
            refusal: AtomicU8::new(SETTLED),
        }
    }

    /// Whether V8 calls the near-heap-limit callback because the allocator just refused an
    /// array buffer, before any collection: V8 does so at the start of its last-resort
    /// collection, whatever the heap's size.
    pub(crate) fn took_refusal(&self) -> bool {
        self.refusal
            .compare_exchange(REFUSED, TOLD, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn refused(&self) {
        // After the callback was told, this is V8's last attempt: nothing waits any more.
        if self
            .refusal
            .compare_exchange(TOLD, SETTLED, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            self.refusal.store(REFUSED, Ordering::SeqCst);
        }
    }

    fn collection_started(&self) {
        let _ = self
            .refusal
            .compare_exchange(REFUSED, SETTLED, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// The bytes the isolate's array buffers hold.
    #[cfg(test)]
    fn held(&self) -> usize {
        self.held.load(Ordering::SeqCst)
    }

    /// Takes `len` bytes from the budget, unless that would go past it.
    fn charge(&self, len: usize) -> bool {
        let mut held = self.held.load(Ordering::SeqCst);
        loop {
            let Some(total) = held.checked_add(len).filter(|&total| total <= self.cap) else {
                return false;
            };
            match self
                .held
                .compare_exchange_weak(held, total, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(current) => held = current,
            }
        }
    }

    fn release(&self, len: usize) {
        self.held.fetch_sub(len, Ordering::SeqCst);
    }
}

/// An allocator of array buffers that holds at most `cap` bytes at once, for one isolate, and
/// the budget it charges.
pub(crate) fn capped_allocator(
    cap: usize,
) -> (v8::SharedRef<v8::Allocator>, Arc<ArrayBufferBudget>) {
    let budget = Arc::new(ArrayBufferBudget::new(cap));
    let handle = Arc::into_raw(Arc::clone(&budget));
    // SAFETY: `handle` comes from `Arc::into_raw`, so it stays valid until `drop_budget` gives
    // it back, which V8 calls once, when the allocator is destroyed; until then the functions of
    // `VTABLE` borrow it as the `&ArrayBufferBudget` they expect. `ArrayBufferBudget` is `Send`
    // and `Sync`, as V8 may call the allocator from any thread.
    let allocator = unsafe { v8::new_rust_allocator(handle, &VTABLE) };
    (allocator.make_shared(), budget)
}

static VTABLE: v8::RustAllocatorVtable<ArrayBufferBudget> = v8::RustAllocatorVtable {
    allocate,
    allocate_uninitialized,
    free,
    drop: drop_budget,
};

fn layout(len: usize) -> Option<Layout> {
    Layout::from_size_align(len, ALIGN).ok()
}

/// What V8 gets for an empty buffer: never read, written or freed.
fn empty() -> *mut c_void {
    std::ptr::without_provenance_mut(ALIGN)
}

/// Allocates `len` bytes charged to `budget`, zeroed or not. Null when the budget or the
/// system refuses: V8 then collects garbage and asks again, and throws a `RangeError` at last.
fn allocate_with(budget: &ArrayBufferBudget, len: usize, zeroed: bool) -> *mut c_void {
    let data = if !budget.charge(len) {
        std::ptr::null_mut()
    } else if len == 0 {
        empty()
    } else if let Some(layout) = layout(len) {
        // SAFETY: `layout` has a non-zero size (`len > 0`) and a valid power-of-two alignment.
        let data = unsafe {
            if zeroed {
                System.alloc_zeroed(layout)
            } else {
                System.alloc(layout)
            }
        };
        if data.is_null() {
            budget.release(len);
        }
        data.cast()
    } else {
        budget.release(len);
        std::ptr::null_mut()
    };
    if data.is_null() {
        budget.refused();
    } else {
        budget.refusal.store(SETTLED, Ordering::SeqCst);
    }
    data
}

/// `ArrayBuffer::Allocator::Allocate`: zeroed memory, or null.
///
/// # Safety
///
/// Called by V8 only, with the budget `capped_allocator` gave it.
unsafe extern "C" fn allocate(budget: &ArrayBufferBudget, len: usize) -> *mut c_void {
    allocate_with(budget, len, true)
}

/// `ArrayBuffer::Allocator::AllocateUninitialized`: memory V8 fills itself, or null.
///
/// # Safety
///
/// Called by V8 only, with the budget `capped_allocator` gave it.
unsafe extern "C" fn allocate_uninitialized(budget: &ArrayBufferBudget, len: usize) -> *mut c_void {
    allocate_with(budget, len, false)
}

/// `ArrayBuffer::Allocator::Free`.
///
/// # Safety
///
/// `data` was returned by `allocate` or `allocate_uninitialized` of this allocator for this
/// same `len`, and is freed once: V8's contract, and V8 15.0 frees a backing store with the
/// length it allocated (`BackingStore::~BackingStore`).
unsafe extern "C" fn free(budget: &ArrayBufferBudget, data: *mut c_void, len: usize) {
    if len != 0
        && let Some(layout) = layout(len)
    {
        // SAFETY: by this function's contract, `data` was allocated by `System` with this very
        // layout (same length, same alignment) and is not used after this call.
        unsafe { System.dealloc(data.cast(), layout) };
    }
    budget.release(len);
}

/// Makes `budget` forget a refusal at the start of every collection of `isolate`, whose array
/// buffers it bounds.
pub(crate) fn track_refusals(isolate: &mut v8::Isolate, budget: &Arc<ArrayBufferBudget>) {
    isolate.add_gc_prologue_callback(
        collection_started,
        Arc::as_ptr(budget).cast_mut().cast(),
        v8::GCType::kGCTypeAll,
    );
}

/// A collection of an isolate starts.
///
/// # Safety
///
/// `data` is the budget of the isolate's allocator, registered by `track_refusals`.
unsafe extern "C" fn collection_started(
    _isolate: v8::UnsafeRawIsolatePtr,
    _type: v8::GCType,
    _flags: v8::GCCallbackFlags,
    data: *mut c_void,
) {
    // SAFETY: `data` points to the budget the isolate's allocator holds a reference to, so it
    // lives at least as long as the isolate, which runs no collection once it is disposed.
    let budget = unsafe { &*data.cast_const().cast::<ArrayBufferBudget>() };
    budget.collection_started();
}

unsafe extern "C" {
    /// `void v8::Isolate::SetStackLimit(uintptr_t)`, of the V8 that rusty_v8 links, which rusty_v8
    /// does not bind. A non-virtual member function: the Itanium C++ ABI of the hosts ai-ptc runs
    /// on (Linux, macOS) calls it as a C function that takes `this` first.
    #[link_name = "_ZN2v87Isolate13SetStackLimitEm"]
    fn v8_isolate_set_stack_limit(isolate: v8::UnsafeRawIsolatePtr, stack_limit: usize);
}

/// Sets the limit of `isolate`'s stack [`SCRIPT_STACK`] bytes below the caller's frame. Call it
/// right before calling into JavaScript, on a thread of [`ISOLATE_THREAD_STACK`] bytes.
#[inline(never)]
pub(crate) fn pin_stack(isolate: &mut v8::Isolate) {
    let frame = 0u8;
    let here = std::hint::black_box(std::ptr::from_ref(&frame)).addr();
    let limit = here.saturating_sub(SCRIPT_STACK);
    // SAFETY: `isolate` is a live isolate this thread uses, and the declaration above matches the
    // function V8 exports. The limit lies within this thread's stack, with room below it: the
    // isolate threads have ISOLATE_THREAD_STACK bytes, far more than SCRIPT_STACK and the frames
    // above this one.
    unsafe { v8_isolate_set_stack_limit(isolate.as_raw_isolate_ptr(), limit) };
}

/// Gives back the budget's reference that `capped_allocator` lent V8.
///
/// # Safety
///
/// Called by V8 once, when the allocator is destroyed, with the pointer `capped_allocator`
/// passed to `new_rust_allocator`.
unsafe extern "C" fn drop_budget(budget: *const ArrayBufferBudget) {
    // SAFETY: `budget` comes from `Arc::into_raw` in `capped_allocator`, and V8 calls this once.
    drop(unsafe { Arc::from_raw(budget) });
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    /// The allocator's functions, as V8 calls them: within the budget, beyond it, and freed.
    #[test]
    fn allocations_are_charged_refused_past_the_budget_and_released() {
        let budget = ArrayBufferBudget::new(MIB);
        // SAFETY: the functions get the budget they are written for, and each pointer is freed
        // once with the length it was allocated with.
        unsafe {
            let a = allocate(&budget, 600 * 1024);
            assert!(!a.is_null());
            assert_eq!(a as usize % ALIGN, 0);
            let bytes = std::slice::from_raw_parts(a.cast::<u8>(), 600 * 1024);
            assert!(bytes.iter().all(|&byte| byte == 0), "zeroed");
            assert_eq!(budget.held(), 600 * 1024);

            // Past the budget: refused, and nothing is charged.
            assert!(allocate(&budget, 500 * 1024).is_null());
            assert!(allocate_uninitialized(&budget, 500 * 1024).is_null());
            assert_eq!(budget.held(), 600 * 1024);

            // What is left fits, to the byte.
            let b = allocate_uninitialized(&budget, 424 * 1024);
            assert!(!b.is_null());
            assert_eq!(budget.held(), MIB);
            assert!(allocate(&budget, 1).is_null());

            free(&budget, a, 600 * 1024);
            assert_eq!(budget.held(), 424 * 1024);
            let c = allocate(&budget, 600 * 1024);
            assert!(!c.is_null());
            free(&budget, b, 424 * 1024);
            free(&budget, c, 600 * 1024);
            assert_eq!(budget.held(), 0);

            // An empty buffer costs nothing, and a length no layout can have is refused.
            let empty = allocate(&budget, 0);
            assert!(!empty.is_null());
            free(&budget, empty, 0);
            assert!(allocate(&ArrayBufferBudget::new(usize::MAX), usize::MAX).is_null());
            assert_eq!(budget.held(), 0);
        }
    }

    fn isolate(cap: usize) -> (v8::OwnedIsolate, Arc<ArrayBufferBudget>) {
        crate::executor::init_v8();
        let (allocator, budget) = capped_allocator(cap);
        let isolate = v8::Isolate::new(
            v8::CreateParams::default()
                .heap_limits(0, 64 * MIB)
                .array_buffer_allocator(allocator),
        );
        (isolate, budget)
    }

    fn run(isolate: &mut v8::OwnedIsolate, source: &str) -> String {
        pin_stack(isolate);
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(let scope, scope);
        let source = v8::String::new(scope, source).unwrap();
        match v8::Script::compile(scope, source, None).and_then(|script| script.run(scope)) {
            Some(value) => value.to_rust_string_lossy(scope),
            None => {
                let exception = scope.exception().unwrap();
                format!("threw {}", exception.to_rust_string_lossy(scope))
            }
        }
    }

    /// In V8: a buffer past the budget is a `RangeError` the script can catch, never an abort of
    /// the process, and buffers the script dropped give their bytes back.
    #[test]
    fn v8_throws_a_range_error_past_the_budget_and_frees_what_is_dropped() {
        let (mut isolate, budget) = isolate(8 * MIB);
        assert_eq!(
            run(&mut isolate, "new ArrayBuffer(2 ** 36).byteLength"),
            "threw RangeError: Array buffer allocation failed"
        );
        assert_eq!(budget.held(), 0);

        assert_eq!(
            run(
                &mut isolate,
                "globalThis.kept = [new Float64Array(2 ** 19), new Uint8Array(2 ** 22)]; \
                 try { new Uint8Array(1) } catch (e) { String(e) }"
            ),
            "RangeError: Array buffer allocation failed"
        );
        assert_eq!(budget.held(), 8 * MIB);

        // Dropped buffers are garbage: V8 collects it before it gives up on an allocation.
        assert_eq!(
            run(
                &mut isolate,
                "globalThis.kept = null; \
                 let n = 0; for (let i = 0; i < 64; i++) n += new Uint8Array(2 ** 21).length; n"
            ),
            (64 * 2 * MIB).to_string()
        );
        isolate.low_memory_notification();
        assert_eq!(budget.held(), 0);
        drop(isolate);
        assert_eq!(Arc::strong_count(&budget), 1, "V8 gave the budget back");
    }

    /// Calls `f` below `kib` KiB of Rust frames.
    fn below(kib: usize, f: &mut dyn FnMut() -> String) -> String {
        if kib == 0 {
            return f();
        }
        let frame = std::hint::black_box([0u8; 1024]);
        let result = below(kib - 1, f);
        std::hint::black_box(&frame);
        result
    }

    const DEPTH: &str = "function f(n) { try { return f(n + 1) } catch { return n } } f(0)";

    /// The depth a script reaches does not depend on the Rust frames above the call into V8:
    /// they change from one build to the next.
    #[test]
    fn the_stack_limit_is_pinned_below_the_call() {
        let depths = std::thread::Builder::new()
            .stack_size(ISOLATE_THREAD_STACK)
            .spawn(|| {
                // A fresh isolate each time: a function V8 already ran has other frames.
                let pinned: Vec<String> = [0, 64, 256]
                    .into_iter()
                    .map(|kib| {
                        let (mut pinned, _) = isolate(MIB);
                        below(kib, &mut || run(&mut pinned, DEPTH))
                    })
                    .collect();
                // V8's own limit, set when the isolate was created: deeper Rust, shallower script.
                let (mut fresh, _) = isolate(MIB);
                let unpinned = below(256, &mut || {
                    v8::scope!(let scope, &mut fresh);
                    let context = v8::Context::new(scope, Default::default());
                    let scope = &mut v8::ContextScope::new(scope, context);
                    let source = v8::String::new(scope, DEPTH).unwrap();
                    let script = v8::Script::compile(scope, source, None).unwrap();
                    script.run(scope).unwrap().to_rust_string_lossy(scope)
                });
                (pinned, unpinned)
            })
            .unwrap()
            .join()
            .unwrap();
        let (pinned, unpinned) = depths;
        assert_eq!(pinned[0], pinned[1], "{pinned:?}");
        assert_eq!(pinned[0], pinned[2], "{pinned:?}");
        assert_ne!(pinned[0], unpinned, "{pinned:?} {unpinned}");
    }
}
