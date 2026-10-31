use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    ptr,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub attempts: usize,
    pub failures: usize,
    /// Change in requested live bytes during the measurement scope.
    pub live_bytes: i128,
    pub peak_bytes: i128,
}

#[derive(Clone, Copy, Default)]
struct State {
    active: bool,
    fail_at: Option<usize>,
    stats: Stats,
}

thread_local! {
    static STATE: Cell<State> = const { Cell::new(State {
        active: false,
        fail_at: None,
        stats: Stats { attempts: 0, failures: 0, live_bytes: 0, peak_bytes: 0 },
    }) };
}

pub struct TestAllocator;

fn should_fail() -> bool {
    STATE
        .try_with(|state| {
            let mut current = state.get();
            if !current.active {
                return false;
            }
            let attempt = current.stats.attempts;
            current.stats.attempts = attempt.saturating_add(1);
            let fail = current.fail_at == Some(attempt);
            if fail {
                current.fail_at = None;
                current.stats.failures = current.stats.failures.saturating_add(1);
            }
            state.set(current);
            fail
        })
        .unwrap_or(false)
}

fn change_bytes(change: i128) {
    let _ = STATE.try_with(|state| {
        let mut current = state.get();
        if current.active {
            current.stats.live_bytes = current.stats.live_bytes.saturating_add(change);
            current.stats.peak_bytes = current.stats.peak_bytes.max(current.stats.live_bytes);
            state.set(current);
        }
    });
}

// SAFETY: Every successful allocation and all matching deallocations use System.
// Tracking uses allocation-free thread-local state and never unwinds in allocator callbacks.
unsafe impl GlobalAlloc for TestAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if should_fail() {
            return ptr::null_mut();
        }
        // SAFETY: GlobalAlloc's caller supplies a valid nonzero layout.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            change_bytes(layout.size() as i128);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if should_fail() {
            return ptr::null_mut();
        }
        // SAFETY: GlobalAlloc's caller supplies a valid nonzero layout.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            change_bytes(layout.size() as i128);
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if should_fail() {
            return ptr::null_mut();
        }
        // SAFETY: The live System allocation and requested size satisfy GlobalAlloc's contract.
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if !replacement.is_null() {
            change_bytes(new_size as i128 - layout.size() as i128);
        }
        replacement
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: The caller returns a live allocation with its original layout to System.
        unsafe { System.dealloc(pointer, layout) };
        change_bytes(-(layout.size() as i128));
    }
}

struct DisableTracking;

impl Drop for DisableTracking {
    fn drop(&mut self) {
        let _ = STATE.try_with(|state| {
            let mut current = state.get();
            current.active = false;
            current.fail_at = None;
            state.set(current);
        });
    }
}

pub fn stats() -> Stats {
    STATE.with(|state| state.get().stats)
}

/// Counts actual allocator calls on this thread and optionally rejects one call.
/// Byte counts describe requested layouts, excluding allocator bookkeeping and RSS.
pub fn measure<T>(fail_at: Option<usize>, operation: impl FnOnce() -> T) -> (T, Stats) {
    STATE.with(|state| {
        assert!(
            !state.get().active,
            "Allocation measurements must be separate scopes"
        );
        state.set(State {
            active: true,
            fail_at,
            stats: Stats::default(),
        });
    });
    let guard = DisableTracking;
    let result = operation();
    let stats = stats();
    drop(guard);
    (result, stats)
}
