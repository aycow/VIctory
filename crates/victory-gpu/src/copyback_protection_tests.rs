//! The fixtures own their mappings and can change protection at known boundaries.

use std::ptr::null;
use std::sync::Barrier;
use std::sync::atomic::{AtomicU8, Ordering};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE,
    MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_GUARD, PAGE_READONLY,
    PAGE_READWRITE, UnmapViewOfFile, VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::{for_mapped_write_regions, for_pieces, write_mapped};

const PAGE: usize = 4 << 10;

struct Private(*mut u8);

impl Private {
    fn new(bytes: usize) -> Self {
        // SAFETY: allocate a private writable range owned by the fixture.
        let base = unsafe { VirtualAlloc(null(), bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        assert!(!base.is_null());
        Self(base.cast())
    }
}

impl Drop for Private {
    fn drop(&mut self) {
        // SAFETY: this is the base of the fixture's original reservation.
        unsafe { VirtualFree(self.0.cast(), 0, MEM_RELEASE) };
    }
}

struct View(MEMORY_MAPPED_VIEW_ADDRESS);

impl View {
    fn base(&self) -> *mut u8 {
        self.0.Value.cast()
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: the fixture owns this view.
        unsafe { UnmapViewOfFile(self.0) };
    }
}

fn aliased_page() -> (View, View) {
    // SAFETY: create a pagefile section and retain both views after closing it.
    unsafe {
        let section =
            CreateFileMappingW(-1isize as _, null(), PAGE_READWRITE, 0, PAGE as u32, null());
        assert!(!section.is_null());
        let first = View(MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, PAGE));
        let second = View(MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, PAGE));
        CloseHandle(section);
        assert!(!first.base().is_null() && !second.base().is_null());
        (first, second)
    }
}

fn protect(base: *mut u8, flags: u32) {
    let mut previous = 0;
    // SAFETY: the tests only pass one committed page of their mapped fixtures.
    let ok = unsafe { VirtualProtect(base.cast(), PAGE, flags, &mut previous) };
    assert_ne!(ok, 0);
}

fn protection(base: *mut u8) -> u32 {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: the output is a local structure and VirtualQuery checks the address.
    let queried = unsafe { VirtualQuery(base.cast(), &mut info, std::mem::size_of_val(&info)) };
    assert_ne!(queried, 0);
    info.Protect
}

#[test]
fn guarded_alias_retains_its_guard_and_shared_bytes() {
    let (guarded, unguarded) = aliased_page();
    let (guarded, visible) = (guarded.base(), unguarded.base());
    // SAFETY: both mappings are writable here, and the fixture owns them.
    unsafe { visible.write_bytes(0x11, PAGE) };
    protect(guarded, PAGE_READWRITE | PAGE_GUARD);

    assert_eq!(write_mapped(guarded, [0x63].as_ptr(), 1), 0);
    assert_eq!(protection(guarded), PAGE_READWRITE | PAGE_GUARD);
    // SAFETY: the second view is unguarded and exposes the same page.
    assert_eq!(unsafe { visible.read() }, 0x11);

    assert_eq!(write_mapped(visible, [0x37, 0x44].as_ptr(), 2), 2);
    assert_eq!(write_mapped(visible.wrapping_add(1), [0x55].as_ptr(), 1), 1);
    assert_eq!(protection(guarded), PAGE_READWRITE | PAGE_GUARD);
    protect(guarded, PAGE_READWRITE);
    // SAFETY: both views are readable after restoring the first view's protection.
    let observed = unsafe { std::slice::from_raw_parts(guarded, 3) };
    assert_eq!(observed, [0x37, 0x55, 0x11]);
}

#[test]
fn exact_writes_preserve_a_concurrent_cpu_byte() {
    let title = Private::new(PAGE);
    // SAFETY: byte 1 is committed and receives only atomic accesses until join.
    let cpu = unsafe { AtomicU8::from_ptr(title.0.add(1)) };
    // SAFETY: byte 2 is writable and disjoint from the fixture's writes.
    unsafe { title.0.add(2).write(0x11) };
    let ready = Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            cpu.store(0x71, Ordering::Relaxed);
            ready.wait();
            for i in 0..65_536 {
                cpu.store(if i % 2 == 0 { 0x72 } else { 0x71 }, Ordering::Relaxed);
            }
        });
        ready.wait();
        for value in (0..64).cycle().take(2_048) {
            assert_eq!(write_mapped(title.0, [value].as_ptr(), 1), 1);
            assert!(matches!(cpu.load(Ordering::Relaxed), 0x71 | 0x72));
        }
    });
    // SAFETY: byte 0 has received the last run and byte 2 was never written.
    let (changed, adjacent) = unsafe { (title.0.read(), title.0.add(2).read()) };
    assert_eq!((changed, adjacent), (63, 0x11));
}

#[test]
fn writable_neighbors_survive_guard_readonly_and_decommit() {
    let title = Private::new(5 * PAGE);
    let base = title.0;
    // SAFETY: all five pages are writable before the protection changes below.
    let (hole, readonly, guarded, last) = unsafe {
        base.write_bytes(0x11, 5 * PAGE);
        (
            base.add(PAGE),
            base.add(2 * PAGE),
            base.add(3 * PAGE),
            base.add(4 * PAGE),
        )
    };
    // SAFETY: the second page is inside the fixture's committed reservation.
    assert_ne!(unsafe { VirtualFree(hole.cast(), PAGE, MEM_DECOMMIT) }, 0);
    protect(readonly, PAGE_READONLY);
    protect(guarded, PAGE_READWRITE | PAGE_GUARD);
    let src = vec![0x56; 5 * PAGE];
    assert_eq!(write_mapped(base, src.as_ptr(), src.len()), 2 * PAGE);
    assert_eq!(protection(guarded), PAGE_READWRITE | PAGE_GUARD);
    assert_eq!(protection(readonly), PAGE_READONLY);
    protect(guarded, PAGE_READWRITE);
    for (page, byte) in [
        (base, 0x56),
        (readonly, 0x11),
        (guarded, 0x11),
        (last, 0x56),
    ] {
        // SAFETY: these pages are committed and now readable.
        let observed = unsafe { std::slice::from_raw_parts(page, PAGE) };
        assert!(observed.iter().all(|&b| b == byte));
    }
}

#[test]
fn a_guard_added_after_query_is_never_retried() {
    let title = Private::new(PAGE);
    // SAFETY: this is the fixture's writable first byte.
    unsafe { title.0.write(0x11) };
    let mut attempts = 0;
    let src = [0x63u8];
    for_mapped_write_regions(title.0 as usize, src.len(), |at, n| {
        attempts += 1;
        protect(title.0, PAGE_READWRITE | PAGE_GUARD);
        let mut done = 0;
        // SAFETY: the source is a local byte. The kernel checks the destination.
        let ok = unsafe {
            WriteProcessMemory(
                GetCurrentProcess(),
                at as _,
                src.as_ptr().cast(),
                n,
                &mut done,
            )
        };
        assert_eq!((ok, done), (0, 0));
    });
    assert_eq!(attempts, 1);
    protect(title.0, PAGE_READWRITE);
    // SAFETY: the fixture owns the page and has restored its protection.
    assert_eq!(unsafe { title.0.read() }, 0x11);
}

/// The previous #387 path for a matched synthetic timing and negative control.
fn original_write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    let write = |at: usize, n: usize| {
        let from = src.wrapping_add(at - dst as usize);
        let mut done = 0;
        // SAFETY: the source fixture covers the requested range; Windows checks the destination.
        unsafe { WriteProcessMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) != 0 }
    };
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, PAGE, |at, n| {
                write(at, n);
            });
        }
    });
}

#[test]
#[ignore = "synthetic timing and baseline"]
fn mapped_write_cost() {
    const TOTAL: usize = 4 << 20;
    const SAMPLES: usize = 7;
    let title = Private::new(PAGE);
    // SAFETY: the fixture owns this private committed page.
    unsafe { title.0.write(0x11) };
    protect(title.0, PAGE_READWRITE | PAGE_GUARD);
    original_write_mapped(title.0, [0x63].as_ptr(), 1);
    let flags = protection(title.0);
    protect(title.0, PAGE_READWRITE);
    // SAFETY: protection was restored before sampling the byte.
    let byte = unsafe { title.0.read() };
    println!(
        "baseline guard remaining={} byte={byte:#x}",
        flags & PAGE_GUARD != 0
    );

    let src = vec![0x63; TOTAL];
    for (case, runs) in [
        ("dense", vec![(0, TOTAL)]),
        (
            "sparse",
            (0..TOTAL).step_by(64 << 10).map(|at| (at, 1)).collect(),
        ),
        (
            "fragmented",
            (0..TOTAL).step_by(32).map(|at| (at, 1)).collect(),
        ),
        ("partial", vec![(0, TOTAL)]),
    ] {
        let mut samples = [Vec::new(), Vec::new()];
        for trial in 0..(SAMPLES + 2) {
            for path in [trial % 2, (trial + 1) % 2] {
                let title = Private::new(TOTAL);
                // SAFETY: fill the fixture's writable range before any decommits.
                unsafe { title.0.write_bytes(0x11, TOTAL) };
                if case == "partial" {
                    for at in (TOTAL / 32..TOTAL).step_by(TOTAL / 16) {
                        // SAFETY: all selected pages are in the fixture's reservation.
                        let ok = unsafe { VirtualFree(title.0.add(at).cast(), PAGE, MEM_DECOMMIT) };
                        assert_ne!(ok, 0);
                    }
                }
                let started = std::time::Instant::now();
                for &(at, bytes) in &runs {
                    // SAFETY: the offsets of each run are within both allocations.
                    let (dst, from) = unsafe { (title.0.add(at), src.as_ptr().add(at)) };
                    if path == 0 {
                        original_write_mapped(dst, from, bytes);
                    } else {
                        write_mapped(dst, from, bytes);
                    }
                }
                let elapsed = started.elapsed();
                // SAFETY: the first page is committed for every workload.
                assert_eq!(unsafe { title.0.read() }, 0x63);
                if trial >= 2 {
                    samples[path].push(elapsed);
                }
            }
        }
        samples.iter_mut().for_each(|runs| runs.sort_unstable());
        let [baseline, candidate] = samples.each_ref().map(|runs| runs[SAMPLES / 2]);
        println!(
            "{case} runs={} baseline={baseline:?} candidate={candidate:?}",
            runs.len()
        );
    }
}
