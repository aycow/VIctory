//! Host copies of title buffers (decision 0007, step 3 and "Copies of title
//! memory"). Title buffers are title memory, which the host GPU cannot
//! read, so a buffer a draw uses gets a persistent host buffer of its size
//! on first use (system memory shaders may also write), and the ranges each
//! submission reads (vertex, index and constant data, buffer views) are
//! brought up to date from title memory before it runs: the 64 KiB pages
//! the title wrote since they were last copied ([`crate::watch`]: write
//! watching, or read-only guards on physical-page views), all of it where
//! neither works. A title GPU address inside the buffer is the same offset
//! in its copy. Ranges shaders write go back to title memory after the
//! submission ([`Shadow::gpu_writes`]).
//!
//! shortcut: only the ranges draws are known to read are brought up to
//! date (vertex and index buffer views, constant buffers in tables, root
//! descriptors, buffer views in tables); buffers read through directly
//! indexed heaps keep what their first use copied.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::host::{HostList, host};
use crate::watch::PAGE;

mod copyback;

/// A title buffer's host copy.
pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    /// Where the title buffer starts and how long it is.
    start: u64,
    len: u64,
    /// Per page of title memory from `start`'s on: the write-watch asking
    /// after which it was last copied (0: never).
    copied: Mutex<Vec<u64>>,
    /// Whether the memory is write-watched: unknown, yes, no.
    watched: AtomicU8,
    /// The copy's bytes before the submission ran, from an offset on,
    /// covering the ranges its shaders write: what the GPU wrote differs.
    before: Mutex<Option<(usize, Vec<u8>)>>,
}

/// Snapshot memory kept for reuse (already touched, so no page faults).
static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// An empty buffer with room for `n` bytes, from the pool if it has one.
fn pooled(n: usize) -> Vec<u8> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= n)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => pool.swap_remove(i),
        None => Vec::with_capacity(n),
    };
    v.clear();
    v
}

/// Returns a snapshot buffer to the pool (at most 8 kept, the largest).
fn give_back(v: Vec<u8>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    if pool.len() > 8 {
        pool.sort_by_key(|v| std::cmp::Reverse(v.capacity()));
        pool.truncate(8);
    }
}

const NOT_WATCHED: u8 = 2;

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// written only on the submitting thread.
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

impl Shadow {
    /// The host GPU address of a title address inside the buffer.
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        self.gpu + (address - self.start)
    }

    /// Bytes from a title address to the buffer's end.
    pub(crate) fn left(&self, address: u64) -> u64 {
        (self.start + self.len).saturating_sub(address)
    }

    /// Offset of a title address from the buffer's start.
    pub(crate) fn offset(&self, address: u64) -> u64 {
        address - self.start
    }

    /// Brings `len` bytes at `address` up to date when `list` is submitted,
    /// and keeps the copy alive until the list is reset.
    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.start + self.len - address);
        list.defer_refresh(self, address as usize, len as usize);
    }

    fn new(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64) -> Shadow {
        let pages = (start + len).div_ceil(PAGE as u64) - start / PAGE as u64;
        Shadow {
            buffer,
            data,
            gpu,
            start,
            len,
            copied: Mutex::new(vec![0; pages as usize]),
            watched: AtomicU8::new(0),
            before: Mutex::default(),
        }
    }

    /// `[address, address + len)` cut to the buffer, as offsets in the copy.
    fn inside(&self, address: usize, len: usize) -> (usize, usize) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let from = address.clamp(start, end);
        (from - start, (address + len).clamp(from, end) - start)
    }

    /// Shaders of `list` may write `[address, address + len)` of the copy
    /// (unordered access): after the list's submission has run, the range
    /// is copied back into title memory, where the title reads it. Call after
    /// the [`Shadow::refresh`] that gives the shaders the title's data.
    ///
    /// shortcut: what shaders wrote is found by keeping the range's bytes
    /// before the submission (a copy per declared range and submission);
    /// a GPU-side dirty mask would save that if big ranges make it slow.
    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize);
    }

    /// Once all CPU refreshes are done, keep the bytes the GPU starts with,
    /// including refreshed title writes inside declared UAV ranges.
    pub(crate) fn snapshot(&self, address: usize, len: usize) {
        let (from, to) = self.inside(address, len);
        // SAFETY: inside the mapping.
        let host = unsafe { std::slice::from_raw_parts(self.data.add(from), to - from) };
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert!(before.is_none());
        let mut bytes = pooled(to - from);
        bytes.extend_from_slice(host);
        *before = Some((from, bytes));
    }

    /// After the submission ran: copies the bytes its shaders wrote back
    /// into title memory (unmapped pages left alone), and only those: the
    /// title's threads write the same buffers while the GPU runs (its
    /// command memory shares the big GPU heap with what shaders write), and
    /// putting back the copy's older bytes there broke the title's own
    /// records (a fence list read as -1, title+0x4e9b54a). Returns the bytes
    /// written.
    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        let snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        if let Some((taken, before)) = &*snapshot
            && *taken <= from
            && to <= taken + before.len()
            && from < to
        {
            let before = &before[from - taken..to - taken];
            // By 64 KiB piece: the copy compared with its bytes before the
            // submission, title memory read only where the GPU wrote.
            const PIECE: usize = 64 << 10;
            const _: () = assert!(PIECE == PAGE);
            let mut title = vec![0u8; PIECE];
            let mut inspected = Vec::new();
            let mut runs = Vec::new();
            let mut at = from;
            while at < to {
                let n = (PIECE - (start + at) % PIECE).min(to - at);
                // SAFETY: inside the mapping; only the submitting thread
                // writes it.
                let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                let old = &before[at - from..at - from + n];
                let title = &mut title[..n];
                if host != old {
                    let (bytes, complete) =
                        copyback::piece(start + at, host, old, title, &mut runs);
                    written += bytes;
                    inspected.push(((start + at) / PAGE, complete));
                }
                at += n;
            }
            // Pages made equal to title memory are current in the copy, so
            // our own write is not taken for the title's; others keep what
            // they had (a title write there shows at the next refresh).
            let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
            let observation = if first < last
                && !inspected.is_empty()
                && self.watched.load(Ordering::Relaxed) != NOT_WATCHED
            {
                crate::watch::writes(first * PAGE, last * PAGE, true).map(|(now, _)| now)
            } else {
                None
            };
            if observation.is_some() {
                // The asking also cleared any title write made after we read
                // a page; a page still equal to title memory now had none,
                // the others stay stale for the next refresh to copy.
                for (page, complete) in &mut inspected {
                    if !*complete || !(first..last).contains(page) {
                        *complete = false;
                        continue;
                    }
                    // SAFETY: a whole page inside the mapping.
                    let host = unsafe {
                        std::slice::from_raw_parts(self.data.add(*page * PAGE - start), PAGE)
                    };
                    *complete = crate::host::read_into((*page * PAGE) as *const u8, &mut title)
                        && title[..] == *host;
                }
            }
            if !inspected.is_empty() {
                let base = start / PAGE;
                let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                for (page, complete) in inspected {
                    copied[page - base] = if complete { observation.unwrap_or(0) } else { 0 };
                }
            }
        }
        written
    }

    /// After every range from this shadow has been copied back.
    pub(crate) fn finish_writes(&self) {
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, bytes)) = snapshot.take() {
            give_back(bytes);
        }
    }

    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched). Earlier batches have already copied back, and this
    /// batch has yet to execute. Returns the bytes copied.
    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return 0;
        }
        if self.watched.load(Ordering::Relaxed) != NOT_WATCHED {
            if let Some((now, written)) = crate::watch::writes(from, to, true) {
                self.watched.store(1, Ordering::Relaxed);
                let copied = self.copy_stale(from, to, now, &written);
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::Checked, (to - from) as u64);
                    add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
                }
                return copied;
            }
            self.watched.store(NOT_WATCHED, Ordering::Relaxed);
        }
        self.copy(from, to);
        to - from
    }

    /// Copies the pages of `[from, to)` written after their last copy
    /// (`written`: last writes by page, in page order; `now`: this
    /// asking), whole pages within the buffer.
    fn copy_stale(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let base = self.start as usize / PAGE;
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let mut writes = written.iter().peekable();
        let (mut bytes, mut run) = (0, None);
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let at = &mut copied[page - base];
            if *at == 0 || write > *at {
                // A page only partly in the range is copied in part: it
                // stays stale for the rest.
                let (start, end) = (self.start as usize, (self.start + self.len) as usize);
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    *at = now;
                }
                run.get_or_insert(page);
            } else if let Some(first) = run.take() {
                bytes += self.copy((first * PAGE).max(from), (page * PAGE).min(to));
            }
        }
        // Cut to the range: next to it shaders may be writing the copy.
        if let Some(first) = run {
            bytes += self.copy((first * PAGE).max(from), ((last + 1) * PAGE).min(to));
        }
        bytes
    }

    /// Copies title memory `[from, to)`, cut to the buffer, into the copy.
    fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let at = unsafe { self.data.add(from - start) };
        crate::host::copy_mapped(at, from as *const u8, to - from, false);
        to - from
    }
}

struct Entry {
    end: u64,
    shadow: Option<Arc<Shadow>>,
}

/// Title buffers by (start address, resource), with their copy once made.
static BUFFERS: Mutex<BTreeMap<(u64, usize), Entry>> = Mutex::new(BTreeMap::new());

fn buffers() -> std::sync::MutexGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A title buffer exists at `start` (`len` bytes).
pub(crate) fn register(resource: usize, start: u64, len: u64) {
    buffers().insert(
        (start, resource),
        Entry {
            end: start + len,
            shadow: None,
        },
    );
}

/// The title released it; lists that still hold its copy keep it alive.
pub(crate) fn unregister(resource: usize, start: u64) {
    buffers().remove(&(start, resource));
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use.
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let mut map = buffers();
    let key = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .map(|(k, _)| *k);
    let Some(key) = key else {
        drop(map);
        return loose(address, end);
    };
    let entry = map.get_mut(&key)?;
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        // Rounded up so that views ending in the buffer's last 256 bytes fit.
        // Shaders write some buffers (unordered access), so not upload memory.
        let (buffer, data) = host()?.shared_buffer(len.next_multiple_of(1 << 16))?;
        crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if len >= 16 << 20 {
            tracing::info!(
                start = format_args!("{:#x}", key.0),
                len,
                "large buffer copied to the host"
            );
        } else {
            tracing::debug!(
                start = format_args!("{:#x}", key.0),
                len,
                "buffer copied to the host"
            );
        }
        entry.shadow = Some(Arc::new(Shadow::new(buffer, data, gpu, key.0, len)));
    }
    entry.shadow.clone()
}

/// Titles also use memory outside any buffer object as GPU memory (Grand
/// Theft Auto VI's interface vertices, from its own allocator). Such memory
/// gets copies of the aligned windows it touches.
/// shortcut: copies of windows are never freed; key them to the title's
/// allocations if memory use grows.
const WINDOW: u64 = 4 << 20;

/// Copies of title memory outside buffers, by (start, length).
static LOOSE: Mutex<BTreeMap<(u64, u64), Arc<Shadow>>> = Mutex::new(BTreeMap::new());

fn loose(address: u64, end: u64) -> Option<Arc<Shadow>> {
    let start = address & !(WINDOW - 1);
    let len = end.next_multiple_of(WINDOW) - start;
    let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(shadow) = map.get(&(start, len)) {
        return Some(shadow.clone());
    }
    let (buffer, data) = host()?.shared_buffer(len)?;
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let shadow = Arc::new(Shadow::new(buffer, data, gpu, start, len));
    map.insert((start, len), shadow.clone());
    Some(shadow)
}

/// For device-removed reports: the copy of title memory whose host range
/// starts closest below host GPU address `va`, and where `va` is in it.
pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers()
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}

#[cfg(test)]
mod copy_back_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READONLY, PAGE_READWRITE,
        VirtualAlloc, VirtualFree, VirtualProtect,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    const PAGES: usize = 64;

    /// Write-watched title memory of `PAGES` pages, freed on drop.
    struct Title(usize);
    impl Title {
        fn new() -> Title {
            // SAFETY: a fresh allocation, freed on drop.
            let p = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    PAGES * PAGE,
                    MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                    PAGE_READWRITE,
                )
            } as usize;
            assert_ne!(p, 0);
            Title(p)
        }
    }
    impl Drop for Title {
        fn drop(&mut self) {
            // The next test's memory may come at the same address.
            crate::watch::forget(self.0, self.0 + PAGES * PAGE);
            // SAFETY: our allocation.
            unsafe { VirtualFree(self.0 as *mut _, 0, MEM_RELEASE) };
        }
    }

    /// A host copy of the title memory, brought up to date, with shaders of
    /// the submission declared to write all of it and byte 0 of every page
    /// written by "the GPU".
    fn submitted(base: usize) -> Option<Shadow> {
        let (buffer, data) = host()?.shared_buffer((PAGES * PAGE) as u64)?;
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        let s = Shadow::new(buffer, data, gpu, base as u64, (PAGES * PAGE) as u64);
        assert_eq!(s.update(base, PAGES * PAGE), PAGES * PAGE);
        s.snapshot(base, PAGES * PAGE);
        for p in 0..PAGES {
            // SAFETY: inside the copy.
            unsafe { s.data.add(p * PAGE).write(1) };
        }
        Some(s)
    }

    fn peek(at: usize) -> u8 {
        // SAFETY: the tests pass addresses inside their memory.
        unsafe { (at as *const u8).read_volatile() }
    }

    #[test]
    #[ignore = "native Windows CPU copyback comparison"]
    fn copyback_dense_sparse_and_fragmented() {
        let bytes = PAGES * PAGE;
        for (pattern, stride, partial) in [
            ("dense", 1, false),
            ("sparse", PAGE, false),
            ("fragmented", 32, false),
            ("partial", 4096, true),
        ] {
            let mut times = Vec::new();
            for run in 0..7 {
                let title = Title::new();
                let shadow = submitted(title.0).expect("D3D12 host");
                for i in (0..bytes).step_by(stride) {
                    // SAFETY: the test owns the mapped bytes and models completed GPU writes.
                    unsafe { shadow.data.add(i).write(1) };
                }
                if partial {
                    for page in (0..PAGES).step_by(4) {
                        // SAFETY: this OS page belongs to the enclosing title allocation.
                        let ok = unsafe {
                            VirtualFree(
                                (title.0 + page * PAGE + 4096) as *mut _,
                                4096,
                                MEM_DECOMMIT,
                            )
                        };
                        assert_ne!(ok, 0);
                    }
                }
                let began = std::time::Instant::now();
                let copied = shadow.copy_back(title.0, bytes);
                let elapsed = began.elapsed();
                if run > 1 {
                    times.push(elapsed);
                }
                for i in (0..bytes).step_by(stride) {
                    if partial && (i / PAGE).is_multiple_of(4) && i % PAGE == 4096 {
                        continue;
                    }
                    assert_eq!(peek(title.0 + i), 1);
                }
                assert_eq!(
                    copied,
                    bytes.div_ceil(stride) - if partial { PAGES / 4 } else { 0 }
                );
                if stride > 1 {
                    assert!((0..PAGES).all(|page| peek(title.0 + page * PAGE + 1) == 0));
                }
            }
            times.sort_unstable();
            eprintln!(
                "{pattern}: {} changed bytes in {bytes}, median {:?}",
                bytes.div_ceil(stride),
                times[2]
            );
        }
    }

    #[test]
    fn copyback_preserves_valid_writes_around_a_hole_or_read_only_page() {
        let title = Title::new();
        let base = title.0;
        let Some(s) = submitted(base) else {
            return;
        };
        let mut old = 0;
        for offset in [4095, 4096, 8191, 8192, 12287, 12288] {
            // SAFETY: the fixture owns the native buffer and models completed GPU writes.
            unsafe { s.data.add(offset).write(9) };
        }
        // SAFETY: these two whole OS pages belong to the test reservation.
        unsafe {
            assert_ne!(VirtualFree((base + 4096) as *mut _, 4096, MEM_DECOMMIT), 0);
            assert_ne!(
                VirtualProtect((base + 8192) as *const _, 4096, PAGE_READONLY, &mut old),
                0
            );
        }
        assert_eq!(s.copy_back(base, PAGE), 3);
        assert_eq!(peek(base), 1);
        assert_eq!(peek(base + 4095), 9);
        assert_eq!(peek(base + 8192), 0);
        assert_eq!(peek(base + 12287), 0);
        assert_eq!(peek(base + 12288), 9);
    }

    /// The title's threads keep writing words no shader writes, in every
    /// page shaders wrote, while the copy-back runs: none goes back to an
    /// older value.
    #[test]
    fn copy_back_does_not_undo_title_writes_made_while_it_runs() {
        let title = Title::new();
        let base = title.0;
        let Some(s) = submitted(base) else {
            return; // no host GPU here
        };
        let stop = AtomicBool::new(false);
        let undone = std::thread::scope(|t| {
            let writer = t.spawn(|| {
                let mut last = [0u32; PAGES];
                let mut undone = 0;
                while !stop.load(Relaxed) {
                    for (p, last) in last.iter_mut().enumerate() {
                        let at = (base + p * PAGE + 64) as *mut u32;
                        // SAFETY: inside the title memory.
                        unsafe {
                            if at.read_volatile() != *last {
                                undone += 1;
                            }
                            *last += 1;
                            at.write_volatile(*last);
                        }
                    }
                }
                undone
            });
            s.copy_back(base, PAGES * PAGE);
            stop.store(true, Relaxed);
            writer.join().unwrap()
        });
        assert_eq!(undone, 0, "title writes undone by the copy-back");
        assert!(
            (0..PAGES).all(|p| peek(base + p * PAGE) == 1),
            "the shader's bytes"
        );
    }

    /// The title writes a page once the copy-back has put the shader's
    /// byte there (its read of the page is done), while later pages are
    /// still being copied back: the next refresh brings the write in.
    #[test]
    fn a_title_write_during_copy_back_reaches_the_copy() {
        let title = Title::new();
        let base = title.0;
        let Some(s) = submitted(base) else {
            return; // no host GPU here
        };
        std::thread::scope(|t| {
            t.spawn(|| {
                while peek(base + PAGE) != 1 {
                    std::hint::spin_loop();
                }
                // SAFETY: inside the title memory.
                unsafe { ((base + 8) as *mut u8).write_volatile(7) };
            });
            s.copy_back(base, PAGES * PAGE);
        });
        assert_eq!(peek(base + 8), 7, "in title memory");
        s.finish_writes();
        s.update(base, PAGE);
        assert_eq!(peek(s.data as usize + 8), 7, "in the copy");
    }
}
