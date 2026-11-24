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
//! Mirrored buffer SRVs in bound heaps also refresh at submission, since
//! a shader may select their index dynamically. Directly indexed UAVs
//! still need read/write tracking independent of conventional table use.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

#[cfg(feature = "diagnostics")]
use crate::copystat::{C, add};
use crate::host::{HostList, host};
use crate::watch::PAGE;

/// A title buffer's host copy.
pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    /// Where the title buffer starts and how long it is.
    start: u64,
    len: u64,
    /// The registration this copy was made for has since been removed.
    superseded: AtomicBool,
    /// Which bytes are current at each page's last observed write generation.
    copied: Mutex<CopyCoverage>,
    /// Bytes in the native copy that we zeroed for a reserved title range.
    /// The mapping is checked afresh before reusing these bytes.
    hole_zeroes: OnceLock<Mutex<ValidRanges>>,
    /// Whether the memory is write-watched: unknown, yes, or wholly unwatched.
    watched: AtomicU8,
    /// Ranges shaders of a submission not yet copied back may be writing:
    /// title memory there is older than the copy.
    pending: Mutex<Vec<(usize, usize)>>,
    /// The copy's bytes before the submission ran, from an offset on,
    /// covering the ranges its shaders write: what the GPU wrote differs.
    before: Mutex<Option<(usize, Vec<u8>)>>,
}

struct CopyCoverage {
    /// Per page of title memory: the write-watch asking after which the
    /// entire page in the shadow was copied (0: never or partial).
    whole: Vec<u64>,
    partial: BTreeMap<usize, PartialPage>,
}

struct PartialPage {
    at: u64,
    coverage: ValidRanges,
}

#[derive(Default)]
struct ValidRanges {
    /// Sorted, disjoint title address ranges copied at this generation.
    ranges: Vec<(usize, usize)>,
}

impl ValidRanges {
    /// Copies gaps in the requested range and adds the range to coverage.
    fn cover(&mut self, from: usize, to: usize, mut copy: impl FnMut(usize, usize)) {
        let first = self.ranges.partition_point(|&(_, end)| end < from);
        if self
            .ranges
            .get(first)
            .is_some_and(|&(start, end)| start <= from && to <= end)
        {
            return;
        }
        let mut last = first;
        let mut next = from;
        let mut covered = (from, to);
        for &(start, end) in &self.ranges[first..] {
            if start > to {
                break;
            }
            if next < start {
                copy(next, start);
            }
            next = next.max(end);
            covered = (covered.0.min(start), covered.1.max(end));
            last += 1;
        }
        if next < to {
            copy(next, to);
        }
        if first == last {
            self.ranges.insert(first, covered);
        } else {
            self.ranges[first] = covered;
            self.ranges.drain(first + 1..last);
        }
    }

    fn discard(&mut self, from: usize, to: usize) {
        let first = self.ranges.partition_point(|&(_, end)| end <= from);
        let last = self.ranges.partition_point(|&(start, _)| start < to);
        if first < last {
            let (left, right) = (self.ranges[first].0, self.ranges[last - 1].1);
            let kept = [
                (left < from).then_some((left, from)),
                (to < right).then_some((to, right)),
            ];
            self.ranges.splice(first..last, kept.into_iter().flatten());
        }
    }
}

impl CopyCoverage {
    fn discard_pages(&mut self, first: usize, last: usize) {
        self.whole[first..=last].fill(0);
        if !self.partial.is_empty() {
            for page in first..=last {
                self.partial.remove(&page);
            }
        }
    }

    /// A GPU write may have changed this part of the host page. Keep
    /// coverage outside it, even when copyback could not read title memory.
    fn discard_piece(&mut self, page: usize, lo: usize, hi: usize, from: usize, to: usize) {
        let at = std::mem::take(&mut self.whole[page]);
        if from == lo && to == hi {
            self.partial.remove(&page);
        } else if at != 0 {
            let ranges = [
                (lo < from).then_some((lo, from)),
                (to < hi).then_some((to, hi)),
            ];
            self.partial.insert(
                page,
                PartialPage {
                    at,
                    coverage: ValidRanges {
                        ranges: ranges.into_iter().flatten().collect(),
                    },
                },
            );
        } else if let Some(partial) = self.partial.get_mut(&page) {
            partial.coverage.discard(from, to);
            if partial.coverage.ranges.is_empty() {
                self.partial.remove(&page);
            }
        }
    }
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

const WATCH_UNKNOWN: u8 = 0;
const WATCHED: u8 = 1;
const NOT_WATCHED: u8 = 2;
const MAX_HOLE_RANGES: usize = 128;

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

    pub(crate) fn superseded(&self) -> bool {
        self.superseded.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn read_u32(&self, offset: usize) -> u32 {
        assert!(offset + 4 <= self.len as usize);
        // SAFETY: `offset..offset + 4` lies within the mapped host buffer.
        unsafe { self.data.add(offset).cast::<u32>().read_unaligned() }
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
            superseded: AtomicBool::new(false),
            copied: Mutex::new(CopyCoverage {
                whole: vec![0; pages as usize],
                partial: BTreeMap::new(),
            }),
            hole_zeroes: OnceLock::new(),
            watched: AtomicU8::new(WATCH_UNKNOWN),
            pending: Mutex::default(),
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
    /// is copied back into title memory, where the title reads it; until
    /// then no refresh copies title memory over it. Call after the
    /// [`Shadow::refresh`] that gives the shaders the title's data.
    ///
    /// shortcut: what shaders wrote is found by keeping the range's bytes
    /// before the submission (a copy per declared range and submission);
    /// a GPU-side dirty mask would save that if big ranges make it slow.
    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize);
    }

    /// At submission, after the range's refresh: a submitted list's shaders
    /// write this range. Its bytes are kept to tell later what the GPU wrote:
    /// one snapshot per copy and submission, grown to cover every range the
    /// submission declares (the world declares overlapping views of its
    /// 174 MB heap dozens of times), in reused memory (fresh snapshots cost
    /// ~2.3 million page faults per world frame).
    pub(crate) fn writing(&self, address: usize, len: usize) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((address, address + len));
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        self.discard_hole_zeroes(start + from, start + to);
        // SAFETY: inside the mapping.
        let host =
            |a: usize, b: usize| unsafe { std::slice::from_raw_parts(self.data.add(a), b - a) };
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let (start, end) = match &*before {
            Some((at, bytes)) => ((*at).min(from), (at + bytes.len()).max(to)),
            None => (from, to),
        };
        if let Some((at, bytes)) = &*before
            && (*at, at + bytes.len()) == (start, end)
        {
            return;
        }
        // Parts newly covered from the copy, the rest as it was taken.
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        let mut grown = pooled(end - start);
        match before.take() {
            Some((at, bytes)) => {
                grown.extend_from_slice(host(start, at));
                grown.extend_from_slice(&bytes);
                grown.extend_from_slice(host(at + bytes.len(), end));
                give_back(bytes);
            }
            None => grown.extend_from_slice(host(start, end)),
        }
        *before = Some((start, grown));
        #[cfg(feature = "diagnostics")]
        {
            add(C::SnapshotBytes, (end - start) as u64);
            add(C::SnapshotUs, started.elapsed().as_micros() as u64);
        }
    }

    /// After the submission ran: copies the bytes its shaders wrote back
    /// into title memory (unmapped pages left alone), and only those: the
    /// title's threads write the same buffers while the GPU runs (its
    /// command memory shares the big GPU heap with what shaders write), and
    /// putting back the copy's older bytes there broke the title's own
    /// records (a fence list read as -1, title+0x4e9b54a). Returns the bytes
    /// written.
    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        if let Some((taken, before)) = &*snapshot
            && *taken <= from
            && to <= taken + before.len()
            && from < to
        {
            let before = &before[from - taken..to - taken];
            #[cfg(feature = "diagnostics")]
            add(C::CopyBackCompared, before.len() as u64);
            // By 64 KiB piece: the copy compared with its bytes before the
            // submission, title memory read only where the GPU wrote.
            const PIECE: usize = 64 << 10;
            let mut title = vec![0u8; PIECE];
            let mut changed_pieces = Vec::new();
            let mut synced = Vec::new();
            let mut at = from;
            while at < to {
                let n = (PIECE - (start + at) % PIECE).min(to - at);
                // SAFETY: inside the mapping; only the submitting thread
                // writes it.
                let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                let old = &before[at - from..at - from + n];
                let title = &mut title[..n];
                let differs = host != old;
                if differs {
                    changed_pieces.push((start + at, start + at + n));
                }
                if differs && crate::host::read_into((start + at) as *const u8, title) {
                    // Title memory with the GPU's bytes put in; the copy
                    // takes the title's bytes in turn.
                    let mut changed = false;
                    for ((t, h), o) in title.iter_mut().zip(host.iter_mut()).zip(old) {
                        if *h != *o && *t != *h {
                            *t = *h;
                            changed = true;
                        }
                        *h = *t;
                    }
                    if changed {
                        crate::watch::writing(start + at, n);
                        crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
                        written += n;
                    }
                    synced.push((start + at) / PAGE);
                }
                at += n;
            }
            // Pages made equal to title memory are current in the copy, so
            // our own write is not taken for the title's; others keep what
            // they had (a title write there shows at the next refresh).
            let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
            if !changed_pieces.is_empty() && self.watched.load(Ordering::Relaxed) != NOT_WATCHED {
                let now = if first < last && !synced.is_empty() {
                    crate::watch::writes(first * PAGE, last * PAGE, true).map(|(now, _)| now)
                } else {
                    None
                };
                let base = start / PAGE;
                let end = start + self.len as usize;
                let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                for (from, to) in changed_pieces {
                    let page = from / PAGE;
                    copied.discard_piece(
                        page - base,
                        (page * PAGE).max(start),
                        ((page + 1) * PAGE).min(end),
                        from,
                        to,
                    );
                }
                if let Some(now) = now {
                    for page in synced.into_iter().filter(|p| (first..last).contains(p)) {
                        let page = page - base;
                        copied.whole[page] = now;
                        copied.partial.remove(&page);
                    }
                }
            }
        }
        // Every submitted step that declared the range is done with it (one
        // submission runs at a time), and the snapshot with the last range.
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|&r| r != (address, address + len));
        if pending.is_empty()
            && let Some((_, bytes)) = snapshot.take()
        {
            give_back(bytes);
        }
        written
    }

    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched). Returns the bytes copied.
    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        self.update_ranges(&[(address, address + len)])
    }

    /// Refresh sorted, disjoint title ranges in one preparation pass.
    /// Subtract pending GPU writes before planning observations so all of
    /// the readable pieces can share a fresh sample.
    pub(crate) fn update_ranges(&self, ranges: &[(usize, usize)]) -> usize {
        let ready = self.ready(ranges);
        self.update_ready(&ready)
    }

    /// The parts of sorted, disjoint title ranges that can be refreshed now:
    /// cut to this copy, without the ranges shaders of the submission may
    /// be writing.
    pub(crate) fn ready(&self, ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let Some((&(first, _), &(_, last))) = ranges.first().zip(ranges.last()) else {
            return Vec::new();
        };
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if pending.is_empty() && start <= first && first < last && last <= end {
            return ranges.to_vec();
        }

        pending.sort_unstable();
        let mut ready = Vec::with_capacity(ranges.len());
        let mut writes = pending.as_slice();
        for &(from, to) in ranges {
            let (mut from, to) = (from.max(start), to.min(end));
            while let Some((&(a, b), rest)) = writes.split_first() {
                if from >= to || a >= to {
                    break;
                }
                if b > from {
                    if from < a {
                        ready.push((from, a));
                    }
                    from = b;
                }
                if b >= to {
                    break;
                }
                writes = rest;
            }
            if from < to {
                ready.push((from, to));
            }
        }
        ready
    }

    /// Whether this copy's memory was found not write-watched.
    pub(crate) fn not_watched(&self) -> bool {
        self.watched.load(Ordering::Relaxed) == NOT_WATCHED
    }

    /// The oldest copy of `[from, to)` (see `watch_floor`), for a question
    /// shared with other copies.
    pub(crate) fn floor(&self, from: usize, to: usize) -> u64 {
        self.watch_floor(from, to)
    }

    /// Brings `[from, to)` (ready, see [`Shadow::ready`]) up to date from an
    /// answer for written pages asked over a span shared with other copies.
    pub(crate) fn answered(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        self.watched.store(WATCHED, Ordering::Relaxed);
        self.update_watched(from, to, now, written)
    }

    pub(crate) fn update_ready(&self, mut ranges: &[(usize, usize)]) -> usize {
        let mut copied = 0;
        let mut allocation: Option<crate::watch::Allocation> = None;
        let mut failed_private_watch: Option<crate::watch::Allocation> = None;
        while let Some((&(from, to), remaining)) = ranges.split_first() {
            if self.watched.load(Ordering::Relaxed) == NOT_WATCHED {
                return copied
                    + ranges
                        .iter()
                        .map(|&(from, to)| self.update_unwatched(from, to))
                        .sum::<usize>();
            }
            if let Some(verified) = failed_private_watch.filter(|a| a.is_private_span(from, to)) {
                copied += self.update_private_fallback(from, to, verified);
                ranges = remaining;
                continue;
            }
            // Sparse requests need no allocation lookup. For dense ranges,
            // bound the candidate group before walking it, even if the
            // shadow spans many separate reservations.
            if remaining.is_empty() || Self::watch_group_len(&ranges[..2]) == 1 {
                copied += self.update_free(from, to);
                ranges = remaining;
                continue;
            }
            let page_start = from / PAGE * PAGE;
            allocation = allocation
                .filter(|a| a.is_live() && a.start <= page_start && page_start < a.end)
                .or_else(|| crate::watch::allocation(page_start));
            let limit = allocation.map_or(ranges.len(), |a| {
                ranges.partition_point(|&(_, end)| end.div_ceil(PAGE) <= a.end / PAGE)
            });
            let count = Self::watch_group_len(&ranges[..limit.max(1)]);
            let group = &ranges[..count];
            let end = group[count - 1].1;
            if allocation.is_some_and(|a| a.is_empty_private(page_start, end)) {
                for &(from, to) in group {
                    copied += self.zero_reserved(from, to);
                }
            } else if count > 1
                && let Some((now, written)) =
                    crate::watch::grouped_writes(from, end, self.watch_floor(from, end), allocation)
            {
                self.watched.store(WATCHED, Ordering::Relaxed);
                // One observation is shared by this group only. GPU write
                // ranges have already been excluded, and no result survives
                // another deferred CPU write, remap, or submission.
                let mut written = written.as_slice();
                for &(from, to) in group {
                    written = &written[written.partition_point(|&(page, _)| page < from / PAGE)..];
                    copied += self.update_watched(from, to, now, written);
                }
            } else if count > 1
                && let Some(verified) = allocation.filter(|a| a.is_private_span(from, end))
            {
                // A failed query inside one verified private reservation need
                // not be retried for every view. Copy only requested bytes,
                // with the usual sparse fallback, for this preparation pass.
                failed_private_watch = Some(verified);
                let start = self.start as usize;
                if self.watched.load(Ordering::Relaxed) == WATCH_UNKNOWN
                    && crate::watch::private_region_covers(start, start + self.len as usize, None)
                {
                    self.watched.store(NOT_WATCHED, Ordering::Relaxed);
                }
                copied += group
                    .iter()
                    .map(|&(from, to)| self.update_private_fallback(from, to, verified))
                    .sum::<usize>();
            } else {
                copied += group
                    .iter()
                    .map(|&(from, to)| self.update_free(from, to))
                    .sum::<usize>();
            }
            ranges = &ranges[count..];
        }
        copied
    }

    /// Oldest generation among still-valid bytes in the sampled pages.
    /// Uninitialized bytes copy regardless of the journal answer.
    fn watch_floor(&self, from: usize, to: usize) -> u64 {
        let base = self.start as usize / PAGE;
        let first = from / PAGE - base;
        let last = to.div_ceil(PAGE) - base;
        let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let whole = copied.whole[first..last]
            .iter()
            .copied()
            .filter(|&at| at != 0);
        let partial = copied.partial.range(first..last).map(|(_, page)| page.at);
        whole.chain(partial).min().unwrap_or(u64::MAX)
    }

    fn watch_group_len(ranges: &[(usize, usize)]) -> usize {
        let (from, to) = ranges[0];
        let first = from / PAGE;
        let mut last = to.div_ceil(PAGE);
        let mut covered = last - first;
        let mut count = 1;
        for &(from, to) in &ranges[1..] {
            let from = from / PAGE;
            let to = to.div_ceil(PAGE);
            let combined = covered + to.saturating_sub(last.max(from));
            // Avoid querying an entire unreferenced gap in a sparse shadow.
            if to - first > combined.saturating_mul(4) {
                break;
            }
            covered = combined;
            last = to;
            count += 1;
        }
        count
    }

    /// [`Shadow::update`] of `[from, to)` (inside the buffer) where no
    /// shader writes.
    fn update_free(&self, from: usize, to: usize) -> usize {
        let watched = self.watched.load(Ordering::Relaxed);
        if watched != NOT_WATCHED {
            let mut region = None;
            if let Some((now, written)) =
                crate::watch::shadow_writes(from, to, self.watch_floor(from, to), &mut region)
            {
                self.watched.store(WATCHED, Ordering::Relaxed);
                return self.update_watched(from, to, now, &written);
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::watch_failed(self as *const Self as usize, to - from);
            // Views of a single shadow can cover distinct title allocations.
            // Cache the first negative result only when one region contains
            // the shadow and the query could not have crossed a boundary.
            let start = self.start as usize;
            if watched == WATCH_UNKNOWN
                && crate::watch::private_region_covers(start, start + self.len as usize, region)
            {
                self.watched.store(NOT_WATCHED, Ordering::Relaxed);
            } else {
                return self.update_regions(from, to, region, true);
            }
        }
        self.update_unwatched(from, to)
    }

    fn update_private_fallback(
        &self,
        from: usize,
        to: usize,
        allocation: crate::watch::Allocation,
    ) -> usize {
        if allocation.fully_committed() {
            self.update_unwatched(from, to)
        } else {
            // Query live extents before reading a partial reservation;
            // there is no reason to attempt a bulk read across known holes.
            self.update_regions(from, to, None, false)
        }
    }

    /// A title buffer can span adjacent Windows reservations. EricPlayZ
    /// identified the repeated full copy after their combined watch fails.
    /// Walk the mapping extents, publishing each successful observation
    /// before examining any later unwatchable extent.
    fn update_regions(
        &self,
        mut from: usize,
        to: usize,
        mut observed: Option<crate::watch::MemoryRegion>,
        try_watch: bool,
    ) -> usize {
        let first = from;
        let mut copied = 0;
        while from < to {
            let Some(region) = observed
                .take()
                .filter(|region| region.contains(from))
                .or_else(|| crate::watch::memory_region(from))
            else {
                return copied + self.update_unwatched(from, to);
            };
            let end = to.min(region.end);
            if region.reserved {
                copied += self.zero_reserved(from, end);
            } else if try_watch
                && region.committed_private
                && (from != first || end < to)
                && let Some((now, written)) =
                    crate::watch::grouped_writes(from, end, self.watch_floor(from, end), None)
            {
                self.watched.store(WATCHED, Ordering::Relaxed);
                copied += self.update_watched(from, end, now, &written);
            } else {
                copied += self.update_unwatched(from, end);
            }
            from = end;
        }
        copied
    }

    fn update_unwatched(&self, from: usize, to: usize) -> usize {
        if self.watched.load(Ordering::Relaxed) != NOT_WATCHED {
            // A failed source read may zero part of a previously valid page.
            self.discard_watched(from, to);
        }
        #[cfg(feature = "diagnostics")]
        {
            add(C::UnwatchedRanges, 1);
            add(C::UnwatchedBytes, (to - from) as u64);
        }
        self.copy(from, to, |_, _| {})
    }

    fn discard_watched(&self, from: usize, to: usize) {
        let base = self.start as usize / PAGE;
        self.copied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .discard_pages(from / PAGE - base, (to - 1) / PAGE - base);
    }

    fn discard_hole_zeroes(&self, from: usize, to: usize) {
        if from < to
            && let Some(zeroes) = self.hole_zeroes.get()
        {
            let mut zeroes = zeroes.lock().unwrap_or_else(|e| e.into_inner());
            zeroes.discard(from, to);
            if zeroes.ranges.len() > MAX_HOLE_RANGES {
                zeroes.ranges.clear();
            }
        }
    }

    /// EricPlayZ identified the repeated copy-and-zero on reserved title
    /// memory. We retain host zeroes for exact, re-queried reserved spans,
    /// independently of the write-watch journal or any texture observer.
    fn zero_reserved(&self, from: usize, to: usize) -> usize {
        let copied = self.fill_hole(from, to);
        if copied != 0 {
            self.discard_watched(from, to);
        }
        copied
    }

    fn fill_hole(&self, from: usize, to: usize) -> usize {
        let mut copied = 0;
        {
            let mut zeroes = self
                .hole_zeroes
                .get_or_init(Mutex::default)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            zeroes.cover(from, to, |from, to| {
                let offset = from - self.start as usize;
                // SAFETY: this portion is inside the native mapping, and
                // submission preparation excluded all pending GPU writes.
                unsafe { self.data.add(offset).write_bytes(0, to - from) };
                copied += to - from;
            });
            // Keep metadata bounded for deeply fragmented title memory.
            if zeroes.ranges.len() > MAX_HOLE_RANGES {
                zeroes.ranges.clear();
            }
        }
        #[cfg(feature = "diagnostics")]
        add(C::Unmapped, copied as u64);
        copied
    }

    fn update_watched(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let copied = self.copy_stale(from, to, now, written);
        #[cfg(feature = "diagnostics")]
        {
            add(C::Checked, (to - from) as u64);
            add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
            add(C::WatchBytes, copied as u64);
        }
        copied
    }

    /// Copies the uncovered bytes of `[from, to)` as of this write-watch
    /// query. Full coverage is compact, and a newer page write invalidates
    /// every partial span of that page.
    fn copy_stale(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let start = self.start as usize;
        let end = start + self.len as usize;
        let base = start / PAGE;
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let mut writes = written.iter().peekable();
        let mut bytes = 0;
        let mut unreadable = Vec::new();
        let mut copy = |from, to| {
            bytes += self.copy(from, to, |from, to| unreadable.push((from, to)));
        };
        let mut run: Option<(usize, usize)> = None;
        let mut queue_copy = |from, to| {
            if let Some((_, end)) = &mut run
                && *end == from
            {
                *end = to;
            } else if let Some((from, to)) = run.replace((from, to)) {
                copy(from, to);
            }
        };
        let last = (to - 1) / PAGE;
        #[cfg(feature = "diagnostics")]
        let (mut initial, mut dirty) = (0, 0);
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let index = page - base;
            let at = copied.whole[index];
            if at != 0 && write <= at {
                // A fresh observation covers the entire tracking page,
                // even if the shader only requested part of it.
                copied.whole[index] = now;
                continue;
            }
            let lo = (page * PAGE).max(start);
            let hi = ((page + 1) * PAGE).min(end);
            let (from, to) = (from.max(lo), to.min(hi));
            let partial = copied.partial.get_mut(&index);
            let valid = partial.as_ref().is_some_and(|p| write <= p.at);
            #[cfg(feature = "diagnostics")]
            if !valid {
                if at == 0 && partial.is_none() {
                    initial += 1;
                } else {
                    dirty += 1;
                }
            }

            if from == lo && to == hi && !valid {
                copied.whole[index] = now;
                copied.partial.remove(&index);
                queue_copy(from, to);
                continue;
            }
            let partial = match partial {
                Some(partial) => partial,
                None => copied.partial.entry(index).or_insert_with(|| PartialPage {
                    at: now,
                    coverage: ValidRanges::default(),
                }),
            };
            if !valid {
                partial.coverage.ranges.clear();
            }
            partial.coverage.cover(from, to, &mut queue_copy);
            partial.at = now;
            if partial.coverage.ranges.as_slice() == [(lo, hi)] {
                copied.whole[index] = now;
                copied.partial.remove(&index);
            } else {
                copied.whole[index] = 0;
            }
        }
        // Cut to the range: next to it shaders may be writing the copy.
        if let Some((from, to)) = run {
            copy(from, to);
        }
        // A mapping can become readable again without setting write watch.
        // Do not leave the zero-filled bytes cached at this generation.
        for (from, to) in unreadable {
            for page in from / PAGE..=(to - 1) / PAGE {
                let lo = (page * PAGE).max(start);
                let hi = ((page + 1) * PAGE).min(end);
                copied.discard_piece(page - base, lo, hi, from.max(lo), to.min(hi));
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            add(C::WatchPages, (last + 1 - from / PAGE) as u64);
            add(C::WatchInitialPages, initial);
            add(C::WatchDirtyPages, dirty);
        }
        bytes
    }

    /// Copies title memory and reports unreadable subranges so write watch
    /// cannot mark them current.
    fn copy(&self, mut from: usize, to: usize, mut unavailable: impl FnMut(usize, usize)) -> usize {
        let start = self.start as usize;
        debug_assert!(start <= from && from < to && to <= start + self.len as usize);
        // SAFETY: every transfer below is within the persistent mapping.
        let dst = |at| unsafe { self.data.add(at - start) };
        if let Some(complete) =
            crate::host::try_copy_mapped(dst(from), from as *const u8, to - from, false)
        {
            self.discard_hole_zeroes(from, to);
            if !complete {
                unavailable(from, to);
            }
            return to - from;
        }

        // GetWriteWatch can succeed across a reserved extent. Reuse zeroes
        // only while it remains reserved and track the copied live bytes.
        let mut copied = 0;
        while from < to {
            let region = crate::watch::memory_region(from);
            let end = region.map_or(to, |region| to.min(region.end));
            if region.is_some_and(|region| region.reserved) {
                copied += self.fill_hole(from, end);
                unavailable(from, end);
            } else {
                self.discard_hole_zeroes(from, end);
                let complete = match region {
                    Some(_) => {
                        crate::host::copy_mapped(dst(from), from as *const u8, end - from, false)
                    }
                    None => crate::host::read_pieces(dst(from), from as *const u8, end - from),
                };
                if !complete {
                    unavailable(from, end);
                }
                copied += end - from;
            }
            from = end;
        }
        copied
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

/// Changes only when a registered native copy loses its title backing.
static RETIREMENTS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn retirement_revision() -> u64 {
    RETIREMENTS.load(Ordering::Acquire)
}

impl Entry {
    fn retire(self) {
        if let Some(shadow) = self.shadow {
            shadow.superseded.store(true, Ordering::Release);
            RETIREMENTS.fetch_add(1, Ordering::Release);
        }
    }
}

/// A title buffer exists at `start` (`len` bytes).
pub(crate) fn register(resource: usize, start: u64, len: u64) {
    if let Some(previous) = buffers().insert(
        (start, resource),
        Entry {
            end: start + len,
            shadow: None,
        },
    ) {
        previous.retire();
    }
}

/// The title released it; lists that still hold its copy keep it alive.
pub(crate) fn unregister(resource: usize, start: u64) {
    if let Some(previous) = buffers().remove(&(start, resource)) {
        previous.retire();
    }
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use.
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    if address < 0x10000 {
        return None;
    }
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
        let (buffer, data) = allocate_buffer(len.next_multiple_of(1 << 16))?;
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

fn allocate_buffer(width: u64) -> Option<(ID3D12Resource, *mut u8)> {
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let allocation = host()?.shared_buffer(width)?;
    #[cfg(feature = "diagnostics")]
    {
        add(C::ShadowAllocations, 1);
        add(C::ShadowAllocationBytes, width);
        add(C::ShadowAllocationUs, started.elapsed().as_micros() as u64);
    }
    Some(allocation)
}

fn loose(address: u64, end: u64) -> Option<Arc<Shadow>> {
    let start = address & !(WINDOW - 1);
    let end = end.max(address.saturating_add(64 << 10));
    let len = end.checked_next_multiple_of(WINDOW)? - start;
    let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(shadow) = map.get(&(start, len)) {
        return Some(shadow.clone());
    }
    if !crate::texture::committed(address as *const u8, 1) {
        return None;
    }
    let (buffer, data) = allocate_buffer(len)?;
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
mod tests {
    use super::*;
    use crate::watch::tests::Reservation;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };

    fn native_byte(shadow: &Shadow, offset: usize) -> u8 {
        assert!(offset < shadow.len as usize);
        // SAFETY: the fixture owns the mapped buffer and has no active GPU work.
        unsafe { shadow.data.add(offset).read() }
    }

    fn shadow_at(address: usize, size: usize) -> Option<Shadow> {
        let (buffer, data) = host()?.shared_buffer(size as u64).unwrap();
        // SAFETY: a live native test buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        Some(Shadow::new(buffer, data, gpu, address as u64, size as u64))
    }

    #[test]
    fn partial_commit_refreshes_the_view_inside_a_reserved_page() {
        use windows_sys::Win32::System::Memory::PAGE_NOACCESS;

        const KERNEL_PAGE: usize = 4096;
        const OFFSET: usize = KERNEL_PAGE + 16;
        // SAFETY: a fresh test reservation, with no committed bytes.
        let title = Reservation(unsafe {
            VirtualAlloc(std::ptr::null(), PAGE, MEM_RESERVE, PAGE_NOACCESS)
        });
        assert!(!title.0.is_null());
        let base = title.0 as usize;
        let Some(shadow) = shadow_at(base, PAGE) else {
            return;
        };
        shadow.update(base + OFFSET, 1);
        assert_eq!(native_byte(&shadow, OFFSET), 0);

        // SAFETY: only the interior 4 KiB page of our reservation is committed.
        let committed = unsafe {
            VirtualAlloc(
                (base + KERNEL_PAGE) as *const _,
                KERNEL_PAGE,
                MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        assert_eq!(committed as usize, base + KERNEL_PAGE);
        // SAFETY: inside the newly committed page.
        unsafe { committed.cast::<u8>().add(16).write(7) };
        shadow.update(base + OFFSET, 1);
        assert_eq!(native_byte(&shadow, OFFSET), 7);
    }

    #[test]
    fn watched_shadow_refreshes_after_reported_decommit_and_recommit() {
        use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};

        let Some((title, shadow)) = fixture(true, 1) else {
            return;
        };
        let base = title.0 as usize;
        let offset = 16;
        assert_eq!(shadow.update(base + offset, 1), 1);
        assert_eq!(shadow.update(base + offset, 1), 0);
        assert_eq!(native_byte(&shadow, offset), 1);

        // Both changes take place between refreshes. No CPU write dirties the
        // original write-watch reservation when its page is committed again.
        // SAFETY: this fixture owns the complete reservation at its base.
        assert_ne!(unsafe { VirtualFree(title.0, 0, MEM_DECOMMIT) }, 0);
        crate::watch::title_memory_allocation_changed(base, PAGE);
        assert_eq!(
            // SAFETY: the address is within the fixture's existing reservation.
            unsafe { VirtualAlloc(title.0, PAGE, MEM_COMMIT, PAGE_READWRITE) },
            title.0
        );
        crate::watch::title_memory_allocation_changed(base, PAGE);
        assert_eq!(shadow.update(base + offset, 1), 1);
        assert_eq!(native_byte(&shadow, offset), 0);
    }

    #[test]
    fn remap_notification_refreshes_a_clean_watched_replacement() {
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        let allocate = |base: usize| {
            // SAFETY: requests a fresh test reservation, optionally at the released address.
            let title = Reservation(unsafe {
                VirtualAlloc(
                    base as *const _,
                    PAGE,
                    MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                    PAGE_READWRITE,
                )
            });
            assert!(!title.0.is_null());
            title
        };
        let title = allocate(0);
        let base = title.0 as usize;
        let Some(shadow) = shadow_at(base, PAGE) else {
            return;
        };
        // SAFETY: both allocations are live.
        unsafe { title.0.cast::<u8>().write(9) };
        assert_eq!(shadow.update(base, 1), 1);
        assert_eq!(native_byte(&shadow, 0), 9);
        drop(title);

        let replacement = allocate(base);
        assert_eq!(replacement.0 as usize, base);
        crate::watch::title_memory_allocation_changed(base, PAGE);
        assert_eq!(shadow.update(base, 1), 1);
        assert_eq!(native_byte(&shadow, 0), 0);
    }

    #[test]
    fn remapping_a_section_at_the_same_address_refreshes_its_shadow() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFileEx,
            UnmapViewOfFile,
        };

        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                // SAFETY: this fixture owns the view.
                if unsafe { UnmapViewOfFile(self.0) } != 0 {
                    crate::watch::title_memory_remapped(self.0.Value as usize, PAGE);
                }
            }
        }

        let map = |base: usize| {
            // SAFETY: creates and maps a fresh pagefile-backed section.
            let view = unsafe {
                let section = CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    0,
                    PAGE as u32,
                    std::ptr::null(),
                );
                assert!(!section.is_null());
                let view = MapViewOfFileEx(section, FILE_MAP_WRITE, 0, 0, PAGE, base as *const _);
                CloseHandle(section);
                view
            };
            assert!(!view.Value.is_null());
            crate::watch::title_memory_remapped(view.Value as usize, PAGE);
            View(view)
        };
        let first = map(0);
        let base = first.0.Value as usize;
        let Some(shadow) = shadow_at(base, PAGE) else {
            return;
        };
        // SAFETY: inside the newly mapped read-write view.
        unsafe { first.0.Value.cast::<u8>().write(9) };
        shadow.update(base, 1);
        assert_eq!(native_byte(&shadow, 0), 9);
        drop(first);

        let replacement = map(base);
        assert_eq!(replacement.0.Value as usize, base);
        shadow.update(base, 1);
        assert_eq!(native_byte(&shadow, 0), 0);
    }
    #[test]
    fn one_refresh_tracks_adjacent_watched_reservations() {
        let (first, second) = crate::watch::tests::adjacent_reservations(PAGE, PAGE, true);
        let start = first.0 as usize;
        let Some(shadow) = shadow_at(start, 2 * PAGE) else {
            return;
        };
        // SAFETY: initialize both native reservations before observing them.
        unsafe {
            first.0.cast::<u8>().write_bytes(1, PAGE);
            second.0.cast::<u8>().write_bytes(2, PAGE);
        }
        assert_eq!(shadow.update(start, 2 * PAGE), 2 * PAGE);
        assert_eq!(shadow.update(start, 2 * PAGE), 0);
        // SAFETY: the second reservation stays live.
        unsafe { second.0.cast::<u8>().add(7).write(9) };
        assert_eq!(shadow.update(start, 2 * PAGE), PAGE);
        assert_eq!(native_byte(&shadow, PAGE + 7), 9);
    }

    #[test]
    fn a_later_unwatchable_region_preserves_earlier_dirty_observations() {
        let (first, second) = crate::watch::tests::adjacent(PAGE, PAGE);
        let start = first.0 as usize;
        let Some(shadow) = shadow_at(start, 2 * PAGE) else {
            return;
        };
        let other_shadow = shadow_at(start, PAGE).unwrap();
        // SAFETY: both reservations are live.
        unsafe {
            first.0.cast::<u8>().write_bytes(1, PAGE);
            second.0.cast::<u8>().write_bytes(2, PAGE);
        }
        assert_eq!(shadow.update(start, 2 * PAGE), 2 * PAGE);
        assert_eq!(other_shadow.update(start, PAGE), PAGE);
        let (before, _) = crate::watch::writes(start, start + PAGE, false).unwrap();
        // SAFETY: both reservations are live.
        unsafe {
            first.0.cast::<u8>().add(7).write(9);
            second.0.cast::<u8>().add(7).write(5);
        }
        assert_eq!(shadow.update(start, 2 * PAGE), 2 * PAGE);
        let (_, still_reported) = crate::watch::writes(start, start + PAGE, false).unwrap();
        assert!(
            still_reported
                .iter()
                .any(|&(page, at)| page == start / PAGE && at > before)
        );
        assert_eq!(shadow.update(start, 2 * PAGE), PAGE);
        // This distinct shadow still sees the dirty page that the first
        // shadow consumed before encountering the unwatchable reservation.
        assert_eq!(other_shadow.update(start, PAGE), PAGE);
        assert_eq!(native_byte(&other_shadow, 7), 9);
        assert_eq!(native_byte(&shadow, 7), 9);
        assert_eq!(native_byte(&shadow, PAGE + 7), 5);
    }

    #[test]
    fn watched_holes_keep_live_coverage_without_rewriting_zeroes() {
        use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, PAGE_NOACCESS, VirtualFree};
        const KERNEL_PAGE: usize = 4096;
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        // SAFETY: the fixture takes ownership of the reservation.
        let address = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                PAGE,
                MEM_RESERVE | MEM_WRITE_WATCH,
                PAGE_NOACCESS,
            )
        };
        assert!(!address.is_null());
        let address = Reservation(address);
        let base = address.0 as usize;
        let commit = |offset| {
            let at = address.0.cast::<u8>().wrapping_add(offset).cast();
            assert_eq!(
                // SAFETY: the target page lies inside the fixture's reservation.
                unsafe { VirtualAlloc(at, KERNEL_PAGE, MEM_COMMIT, PAGE_READWRITE) },
                at
            );
            at.cast::<u8>()
        };
        let first = commit(0);
        let last = commit(2 * KERNEL_PAGE);
        // SAFETY: both pages were committed writable above.
        unsafe {
            first.write(7);
            last.write(9);
        }
        let Some(shadow) = shadow_at(base, PAGE) else {
            return;
        };
        assert_eq!(shadow.update(base, PAGE), PAGE);
        assert_eq!(shadow.update(base, PAGE), 0);
        assert_eq!(native_byte(&shadow, 0), 7);
        assert_eq!(native_byte(&shadow, KERNEL_PAGE), 0);
        assert_eq!(native_byte(&shadow, 2 * KERNEL_PAGE), 9);

        let middle = commit(KERNEL_PAGE);
        // SAFETY: the newly committed page is writable.
        unsafe { middle.write(11) };
        assert_eq!(shadow.update(base, PAGE), 3 * KERNEL_PAGE);
        assert_eq!(shadow.update(base, PAGE), 0);
        assert_eq!(native_byte(&shadow, KERNEL_PAGE), 11);

        assert_ne!(
            // SAFETY: this decommits only the previous first writable page.
            unsafe { VirtualFree(first.cast(), KERNEL_PAGE, MEM_DECOMMIT) },
            0
        );
        crate::watch::title_memory_remapped(base, KERNEL_PAGE);
        assert_eq!(shadow.update(base, PAGE), 3 * KERNEL_PAGE);
        assert_eq!(shadow.update(base, PAGE), 0);
        assert_eq!(native_byte(&shadow, 0), 0);
        assert_eq!(native_byte(&shadow, KERNEL_PAGE), 11);
        assert_eq!(native_byte(&shadow, 2 * KERNEL_PAGE), 9);
    }

    #[test]
    fn reserved_prefix_does_not_hide_a_committed_suffix() {
        use windows_sys::Win32::System::Memory::PAGE_NOACCESS;
        const KERNEL_PAGE: usize = 4096;
        // SAFETY: a live reservation with only its second kernel page committed.
        let reserved = unsafe { VirtualAlloc(std::ptr::null(), PAGE, MEM_RESERVE, PAGE_NOACCESS) };
        assert!(!reserved.is_null());
        let reserved = Reservation(reserved);
        let start = reserved.0 as usize;
        // SAFETY: the address lies in the fixture's reserved span.
        let suffix = unsafe {
            VirtualAlloc(
                reserved.0.cast::<u8>().add(KERNEL_PAGE).cast(),
                KERNEL_PAGE,
                MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        assert!(!suffix.is_null());
        let Some(shadow) = shadow_at(start, PAGE) else {
            return;
        };
        // SAFETY: the suffix was committed and the shadow mapping is live.
        unsafe { suffix.cast::<u8>().write(5) };
        assert_eq!(shadow.update(start, 2 * KERNEL_PAGE), 2 * KERNEL_PAGE);
        // SAFETY: the same committed suffix remains live.
        unsafe { suffix.cast::<u8>().write(9) };
        assert_eq!(shadow.update(start, 2 * KERNEL_PAGE), KERNEL_PAGE);
        assert_eq!(native_byte(&shadow, 0), 0);
        assert_eq!(native_byte(&shadow, KERNEL_PAGE), 9);
    }

    #[test]
    fn reserved_hole_rechecks_mapping_and_preserves_pending_gpu_bytes() {
        use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
        let (first, second) = crate::watch::tests::adjacent(PAGE, PAGE);
        let start = first.0 as usize;
        let reserved = second.0 as usize;
        let Some(shadow) = shadow_at(start, 2 * PAGE) else {
            return;
        };
        // SAFETY: both reservations are live and private.
        unsafe {
            first.0.cast::<u8>().write_bytes(1, PAGE);
            second.0.cast::<u8>().write_bytes(7, PAGE);
        }
        assert_eq!(shadow.update(start, PAGE), PAGE);
        assert_eq!(shadow.update(reserved, PAGE), PAGE);
        assert_eq!(native_byte(&shadow, PAGE + 5), 7);
        // SAFETY: the fixture owns the reservation being decommitted.
        assert_ne!(unsafe { VirtualFree(second.0, PAGE, MEM_DECOMMIT) }, 0);
        assert_eq!(shadow.update(reserved, PAGE), PAGE);
        assert_eq!(shadow.update(reserved, PAGE), 0);
        assert_eq!(native_byte(&shadow, PAGE + 5), 0);

        shadow.writing(reserved + 4, 8);
        // SAFETY: simulate the pending GPU store on the mapped shadow.
        unsafe { shadow.data.add(PAGE + 5).write(3) };
        assert_eq!(shadow.update(reserved, PAGE), 0);
        assert_eq!(native_byte(&shadow, PAGE + 5), 3);
        assert_eq!(shadow.copy_back(reserved + 4, 8), 0);
        assert_eq!(shadow.update(reserved, PAGE), 8);
        assert_eq!(native_byte(&shadow, PAGE + 5), 0);

        // SAFETY: the address is still part of the fixture's reservation.
        let recommitted = unsafe { VirtualAlloc(second.0, PAGE, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(recommitted, second.0);
        // SAFETY: the page was recommitted as writable above.
        unsafe { second.0.cast::<u8>().add(5).write(9) };
        assert_eq!(shadow.update(reserved, PAGE), PAGE);
        assert_eq!(native_byte(&shadow, PAGE + 5), 9);
    }

    #[test]
    fn placeholder_map_unmap_and_remap_refresh_new_contents() {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS,
            UnmapViewOfFile2, VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        struct Placeholder {
            address: Reservation,
            section: HANDLE,
            mapped: bool,
        }
        impl Placeholder {
            fn map(&mut self) {
                // SAFETY: the owned placeholder and section are both live.
                let view = unsafe {
                    MapViewOfFile3(
                        self.section,
                        GetCurrentProcess(),
                        self.address.0,
                        0,
                        PAGE,
                        MEM_REPLACE_PLACEHOLDER,
                        PAGE_READWRITE,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                assert_eq!(view.Value, self.address.0);
                self.mapped = true;
                crate::watch::title_memory_remapped(self.address.0 as usize, PAGE);
            }

            fn unmap(&mut self) {
                assert_ne!(
                    // SAFETY: this view was installed into our placeholder.
                    unsafe {
                        UnmapViewOfFile2(
                            GetCurrentProcess(),
                            MEMORY_MAPPED_VIEW_ADDRESS {
                                Value: self.address.0,
                            },
                            MEM_PRESERVE_PLACEHOLDER,
                        )
                    },
                    0
                );
                self.mapped = false;
                crate::watch::title_memory_remapped(self.address.0 as usize, PAGE);
            }
        }
        impl Drop for Placeholder {
            fn drop(&mut self) {
                if self.mapped {
                    self.unmap();
                }
                // SAFETY: the fixture owns this handle.
                assert_ne!(unsafe { CloseHandle(self.section) }, 0);
            }
        }
        // SAFETY: both allocations belong to this fixture.
        let address = unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                PAGE,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            )
        };
        assert!(!address.is_null());
        let address = Reservation(address);
        // SAFETY: the anonymous section is created with valid arguments.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                PAGE as u32,
                std::ptr::null(),
            )
        };
        assert!(!section.is_null());
        let mut placeholder = Placeholder {
            address,
            section,
            mapped: false,
        };
        let base = placeholder.address.0 as usize;
        let Some(shadow) = shadow_at(base, PAGE) else {
            return;
        };
        assert_eq!(shadow.update(base, PAGE), PAGE);
        assert_eq!(shadow.update(base, PAGE), 0);

        placeholder.map();
        // SAFETY: the mapped view is writable before the guard is installed.
        unsafe { placeholder.address.0.cast::<u8>().write(7) };
        assert_eq!(shadow.update(base, PAGE), PAGE);
        assert_eq!(native_byte(&shadow, 0), 7);
        placeholder.unmap();
        assert_eq!(shadow.update(base, PAGE), PAGE);
        assert_eq!(shadow.update(base, PAGE), 0);
        assert_eq!(native_byte(&shadow, 0), 0);

        // A new view of the original section recovers its previous contents.
        placeholder.map();
        assert_eq!(shadow.update(base, PAGE), PAGE);
        assert_eq!(native_byte(&shadow, 0), 7);
    }

    #[test]
    #[ignore = "synthetic performance measurement"]
    fn benchmark_reserved_hole_refreshes() {
        use windows_sys::Win32::System::Memory::PAGE_NOACCESS;
        const SIZE: usize = 32 << 20;
        const REFRESHES: usize = 16;
        // SAFETY: the fixture owns the reservation for the entire run.
        let reserved = unsafe { VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS) };
        assert!(!reserved.is_null());
        let reserved = Reservation(reserved);
        let base = reserved.0 as usize;
        let Some(shadow) = shadow_at(base, SIZE) else {
            return;
        };
        let first = shadow.update(base, SIZE);
        let started = std::time::Instant::now();
        let copied: usize = (0..REFRESHES).map(|_| shadow.update(base, SIZE)).sum();
        eprintln!(
            "reserved_bytes={SIZE} refreshes={REFRESHES} first={first} copied={copied} elapsed_ms={:.2}",
            started.elapsed().as_secs_f64() * 1000.0,
        );
    }

    #[test]
    #[ignore = "synthetic performance measurement"]
    fn benchmark_split_reservation_refreshes() {
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, PAGE_NOACCESS, VirtualFree};
        const REGIONS: usize = 128;
        const REFRESHES: usize = 32;
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        let size = REGIONS * PAGE;
        let reservations = (0..16)
            .find_map(|_| {
                // SAFETY: reserve an address span before attempting separate allocations.
                let span =
                    unsafe { VirtualAlloc(std::ptr::null(), size, MEM_RESERVE, PAGE_NOACCESS) };
                assert!(!span.is_null());
                // SAFETY: the fixture owns this reservation.
                assert_ne!(unsafe { VirtualFree(span, 0, MEM_RELEASE) }, 0);
                let mut allocations = Vec::new();
                for i in 0..REGIONS {
                    // SAFETY: each named address is aligned and the returned reservation is owned.
                    let at = unsafe {
                        VirtualAlloc(
                            span.cast::<u8>().wrapping_add(i * PAGE).cast(),
                            PAGE,
                            MEM_COMMIT | MEM_RESERVE | MEM_WRITE_WATCH,
                            PAGE_READWRITE,
                        )
                    };
                    if at.is_null() {
                        return None;
                    }
                    allocations.push(Reservation(at));
                    // SAFETY: the new reservation is committed and writable.
                    unsafe { at.cast::<u8>().write_bytes(1, PAGE) };
                }
                Some(allocations)
            })
            .expect("adjacent test reservations");
        let base = reservations[0].0 as usize;
        let Some(shadow) = shadow_at(base, size) else {
            return;
        };
        let first = shadow.update(base, size);
        let started = std::time::Instant::now();
        let copied: usize = (0..REFRESHES)
            .map(|i| {
                // SAFETY: one distinct dirty page per refresh.
                unsafe { reservations[i].0.cast::<u8>().write(2) };
                shadow.update(base, size)
            })
            .sum();
        eprintln!(
            "watched_adjacent={REGIONS} refreshes={REFRESHES} first={first} copied={copied} elapsed_ms={:.2}",
            started.elapsed().as_secs_f64() * 1000.0,
        );
    }

    fn fixture(watched: bool, pages: usize) -> Option<(Reservation, Shadow)> {
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        let host = host()?;
        let size = pages * PAGE;
        let (buffer, data) = host.shared_buffer(size as u64).unwrap();
        // SAFETY: a private test reservation, initialized before the first refresh.
        let allocation = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                size,
                MEM_RESERVE | MEM_COMMIT | if watched { MEM_WRITE_WATCH } else { 0 },
                PAGE_READWRITE,
            )
        };
        assert!(!allocation.is_null());
        let allocation = Reservation(allocation);
        // SAFETY: both the title reservation and host buffer are live.
        unsafe { allocation.0.cast::<u8>().write_bytes(1, size) };
        let address = allocation.0 as u64;
        // SAFETY: the buffer is a live native resource.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        Some((
            allocation,
            Shadow::new(buffer, data, gpu, address, size as u64),
        ))
    }

    #[test]
    fn watched_partial_refresh_reuses_only_copied_bytes() {
        let Some((title, shadow)) = fixture(true, 1) else {
            return;
        };
        let address = title.0 as usize;
        assert_eq!(shadow.update(address + 16, 16), 16);
        assert_eq!(shadow.update(address + 16, 16), 0);
        assert_eq!(shadow.update(address + 24, 24), 16);
        assert_eq!(shadow.update(address + 64, 8), 8);
        assert_eq!(shadow.update(address, PAGE), PAGE - 40);
        assert_eq!(shadow.update(address, PAGE), 0);

        // SAFETY: live test reservation with no concurrent writer.
        unsafe { title.0.cast::<u8>().add(20).write(9) };
        assert_eq!(shadow.update(address + 16, 16), 16);
        assert_eq!(shadow.update(address + 16, 16), 0);
        assert_eq!(shadow.update(address + 64, 8), 8);
        assert_eq!(native_byte(&shadow, 20), 9);
    }

    #[test]
    fn unwatched_partial_refresh_sees_later_title_write() {
        let Some((title, shadow)) = fixture(false, 1) else {
            return;
        };
        let address = title.0 as usize + 37;
        assert_eq!(shadow.update(address, 11), 11);
        // SAFETY: the source and destination are live and the GPU is idle.
        unsafe { (address as *mut u8).write(9) };
        assert_eq!(shadow.update(address, 11), 11);
        assert_eq!(native_byte(&shadow, 37), 9);
    }

    #[test]
    fn watched_partial_refresh_retries_an_unreadable_source() {
        use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, VirtualProtect};

        let Some((title, shadow)) = fixture(true, 1) else {
            return;
        };
        let address = title.0 as usize + 4096;
        let protect = |protection| {
            let mut previous = 0;
            assert_ne!(
                // SAFETY: this page is inside the live private reservation.
                unsafe { VirtualProtect(address as *const _, 4096, protection, &mut previous) },
                0
            );
        };
        protect(PAGE_NOACCESS);
        assert_eq!(shadow.update(address, 16), 16);
        assert_eq!(native_byte(&shadow, 4096), 0);
        protect(PAGE_READWRITE);

        // Restoring access does not change the bytes or set write watch.
        assert_eq!(shadow.update(address, 16), 16);
        assert_eq!(native_byte(&shadow, 4096), 1);
        assert_eq!(shadow.update(address, 16), 0);
    }

    #[test]
    fn partial_refresh_preserves_pending_gpu_writes() {
        let Some((title, shadow)) = fixture(true, 1) else {
            return;
        };
        let address = title.0 as usize;
        assert_eq!(shadow.update(address, PAGE), PAGE);
        shadow.writing(address + 32, 8);
        // SAFETY: this emulates one GPU write into an idle, mapped buffer.
        unsafe { shadow.data.add(34).write(7) };
        // SAFETY: the title reservation remains live and writable.
        unsafe { title.0.cast::<u8>().add(20).write(9) };

        assert_eq!(shadow.update(address + 16, 32), 24);
        assert_eq!(shadow.update(address + 16, 32), 0);
        assert_eq!(native_byte(&shadow, 20), 9);
        assert_eq!(native_byte(&shadow, 34), 7);

        assert_eq!(shadow.copy_back(address + 32, 8), 8);
        // SAFETY: copyback finished and the title reservation remains live.
        assert_eq!(unsafe { title.0.cast::<u8>().add(34).read() }, 7);
        // SAFETY: copyback has finished and the title reservation remains writable.
        unsafe { title.0.cast::<u8>().add(34).write(5) };
        assert_eq!(shadow.update(address + 32, 8), 8);
        assert_eq!(native_byte(&shadow, 34), 5);
    }

    #[test]
    fn grouped_watch_reports_gap_writes_and_excludes_pending_bytes() {
        let Some((title, shadow)) = fixture(true, 3) else {
            return;
        };
        let base = title.0 as usize;
        let last = base + 2 * PAGE;
        let ranges = [(base + 16, base + 48), (last + 8, last + 24)];
        assert_eq!(Shadow::watch_group_len(&ranges), 2);
        assert_eq!(shadow.update_ranges(&ranges), 48);
        let (before, _) = crate::watch::writes(base + PAGE, last, false).unwrap();

        shadow.writing(base + 24, 8);
        // SAFETY: one simulated GPU store and three stores to the test title.
        unsafe {
            shadow.data.add(26).write(7);
            title.0.cast::<u8>().add(20).write(9);
            title.0.cast::<u8>().add(PAGE + 4).write(3);
            title.0.cast::<u8>().add(2 * PAGE + 9).write(5);
        }
        assert_eq!(shadow.update_ranges(&ranges), 40);
        assert_eq!(shadow.update_ranges(&ranges), 0);
        // The grouped query consumes the gap's kernel write-watch bit, so
        // a later overlapping shadow must still see it in the journal.
        let (_, written) = crate::watch::writes(base + PAGE, last, false).unwrap();
        assert!(
            written
                .iter()
                .any(|&(page, at)| page == base / PAGE + 1 && at > before)
        );
        // SAFETY: the queried bytes are inside this fixture's mapped buffer.
        unsafe {
            assert_eq!(shadow.data.add(20).read(), 9);
            assert_eq!(shadow.data.add(26).read(), 7);
            assert_eq!(shadow.data.add(2 * PAGE + 9).read(), 5);
        }
        assert_eq!(shadow.copy_back(base + 24, 8), 8);
    }

    #[test]
    fn grouped_watch_keeps_changes_for_the_oldest_shadow_coverage() {
        let Some((title, shadow)) = fixture(true, 3) else {
            return;
        };
        let base = title.0 as usize;
        let last = base + 2 * PAGE;
        let Some(other) = shadow_at(base, 3 * PAGE) else {
            return;
        };
        let ranges = [(base + 8, base + 16), (last + 8, last + 16)];
        assert_eq!(shadow.update_ranges(&ranges), 16);
        assert_eq!(other.update(last + 8, 8), 8);

        // SAFETY: both bytes belong to the live watched title reservation.
        unsafe {
            title.0.cast::<u8>().add(8).write(3);
            title.0.cast::<u8>().add(2 * PAGE + 8).write(9);
        }
        // The other shadow resets the second page's native dirty bit.
        assert_eq!(other.update(last + 8, 8), 8);
        assert_eq!(shadow.update(base + 8, 8), 8);
        assert_eq!(shadow.update_ranges(&ranges), 8);
        assert_eq!(native_byte(&shadow, 8), 3);
        assert_eq!(native_byte(&shadow, 2 * PAGE + 8), 9);

        // A new preparation phase must observe another CPU write.
        // SAFETY: this is the same still-live title reservation.
        unsafe { title.0.cast::<u8>().add(8).write(11) };
        assert_eq!(shadow.update_ranges(&ranges), 8);
        assert_eq!(native_byte(&shadow, 8), 11);
        assert_eq!(shadow.update_ranges(&ranges), 0);
    }

    #[test]
    fn grouped_reserved_reads_see_a_later_commit() {
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        // SAFETY: the fixture acquires and owns this private reservation.
        let title = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * PAGE,
                MEM_RESERVE | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        };
        assert!(!title.is_null());
        let title = Reservation(title);
        let base = title.0 as usize;
        let Some(shadow) = shadow_at(base, 2 * PAGE) else {
            return;
        };
        let ranges = [(base + 8, base + 16), (base + PAGE + 8, base + PAGE + 16)];
        assert_eq!(shadow.update_ranges(&ranges), 16);
        assert_eq!(shadow.update_ranges(&ranges), 0);
        assert_eq!(native_byte(&shadow, 8), 0);
        assert_eq!(native_byte(&shadow, PAGE + 8), 0);

        // SAFETY: the requested page is inside the reservation we own.
        let live =
            unsafe { VirtualAlloc((base + PAGE) as *const _, PAGE, MEM_COMMIT, PAGE_READWRITE) };
        assert_eq!(live as usize, base + PAGE);
        crate::watch::title_memory_allocation_changed(base + PAGE, PAGE);
        // SAFETY: this page is now committed and writable.
        unsafe { live.cast::<u8>().add(8).write(7) };
        assert_eq!(shadow.update_ranges(&ranges), 8);
        assert_eq!(native_byte(&shadow, 8), 0);
        assert_eq!(native_byte(&shadow, PAGE + 8), 7);
        assert_eq!(shadow.update_ranges(&ranges), 0);
    }

    #[test]
    fn grouped_watch_keeps_mixed_private_reservations_separate() {
        let Some(host) = host() else {
            return;
        };
        let (watched, unwatched) = crate::watch::tests::adjacent(2 * PAGE, PAGE);
        let (base, ordinary) = (watched.0 as usize, unwatched.0 as usize);
        // SAFETY: the adjacent fixture owns both reservations.
        unsafe {
            watched.0.cast::<u8>().write_bytes(1, 2 * PAGE);
            unwatched.0.cast::<u8>().write_bytes(2, PAGE);
        }
        let len = 3 * PAGE as u64;
        let (buffer, data) = host.shared_buffer(len).unwrap();
        // SAFETY: the buffer is a live native resource.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        let shadow = Shadow::new(buffer, data, gpu, base as u64, len);
        let ranges = [
            (base + 16, base + 32),
            (base + PAGE + 8, base + PAGE + 24),
            (ordinary + 16, ordinary + 32),
            (ordinary + 64, ordinary + 80),
        ];
        assert_eq!(Shadow::watch_group_len(&ranges), 4);
        assert_eq!(shadow.update_ranges(&ranges), 64);
        // SAFETY: the unwatched title reservation remains live.
        unsafe { unwatched.0.cast::<u8>().add(16).write(9) };
        assert_eq!(shadow.update_ranges(&ranges), 32);
        assert_eq!(native_byte(&shadow, 2 * PAGE + 16), 9);
    }

    #[test]
    #[ignore = "synthetic performance measurement"]
    fn benchmark_partial_shadow_refreshes() {
        const PAGES: usize = 128;
        const RANGES: usize = 32;
        const SUBMISSIONS: usize = 16;
        const RANGE_BYTES: usize = 64;
        for (watched, pending, grouped) in [
            (true, false, false),
            (true, false, true),
            (false, false, false),
            (false, false, true),
            (true, true, false),
            (true, true, true),
        ] {
            let Some((title, shadow)) = fixture(watched, PAGES) else {
                return;
            };
            let address = title.0 as usize;
            let ranges: Vec<_> = (0..PAGES)
                .flat_map(|page| (0..RANGES).map(move |range| address + page * PAGE + range * 128))
                .map(|start| (start, start + RANGE_BYTES))
                .collect();
            let update = || {
                if grouped {
                    shadow.update_ranges(&ranges)
                } else {
                    ranges
                        .iter()
                        .map(|&(start, end)| shadow.update(start, end - start))
                        .sum()
                }
            };
            let initial = update();
            let started = std::time::Instant::now();
            let mut copied = 0;
            for submission in 0..SUBMISSIONS {
                let page = address + submission * PAGE;
                // SAFETY: test allocation, with one CPU write per submission.
                unsafe { (page as *mut u8).write(2) };
                if pending {
                    shadow.writing(page, RANGE_BYTES / 2);
                }
                copied += update();
                if pending {
                    shadow.copy_back(page, RANGE_BYTES / 2);
                }
            }
            eprintln!(
                "watched={watched} pending={pending} grouped={grouped} initial_bytes={initial} copied_bytes={copied} \
                 requests={} elapsed_ms={:.2}",
                SUBMISSIONS * PAGES * RANGES,
                started.elapsed().as_secs_f64() * 1000.0,
            );
        }
    }
}
