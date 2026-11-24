//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed, or when a bindless
//! `ResourceDescriptorHeap[]` draw or dispatch materializes and refreshes
//! deferred buffer, constant-buffer, and placed-texture slots.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{VIEW_TAG, View, stale_views, view_at};
use crate::host::host;

pub(crate) struct HostHeap {
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic` more.
    count: u32,
    /// Dynamic slots past `count` (none in sampler heaps): [`DYNAMIC`]
    /// where the host takes them, else [`MIN_DYNAMIC`].
    dynamic: u32,
    sampler: bool,
    /// The record (view index and generation) each slot was last made from.
    made: Mutex<Vec<u64>>,
    /// The host resource each slot views, held while the slot does: shaders
    /// index heaps directly, so a slot the title has not rewritten since it
    /// released the resource may still be read, and a freed host resource
    /// read there is a GPU page fault (the console reads stale memory).
    kept: Mutex<Vec<Option<ID3D12Resource>>>,
    /// Where the search for free dynamic slots starts, and which are taken
    /// (by lists not yet reset).
    next: Mutex<(u32, Vec<bool>)>,
    /// Populated buffer, constant-buffer, and placed-texture slots (`< count`)
    /// tracked for bindless `ResourceDescriptorHeap[]` materialization.
    bindless: Mutex<Bindless>,
    /// Odd while preparing texture slots. A stale submission snapshot may
    /// retire requests only if this recording generation stayed even.
    recording: AtomicU64,
    /// Placed-texture slots are waiting to be prepared at recording.
    record_hint: std::sync::atomic::AtomicBool,
    /// The placed-texture epoch, and the frame (plus one), recording last
    /// looked for evicted textures at.
    record_looked: [AtomicU64; 2],
    /// The frame the oldest uploaded texture is due a look again in.
    recheck_at: AtomicU64,
}

// SAFETY: D3D12 descriptor heaps and resources are free-threaded; mutable
// slot tracking is synchronized behind mutexes and atomics.
unsafe impl Send for HostHeap {}
// SAFETY: as above.
unsafe impl Sync for HostHeap {}

const BOUNCE_CHUNK: u64 = 256 << 10;

/// Persistent 256-byte-aligned bounce slice for a misaligned bindless CBV.
struct BounceSlot {
    gpu: u64,
    cpu: usize,
    cap: u32,
    resource: ID3D12Resource,
    /// Last `watch::writes` asking number copied into this slice (0 = dirty).
    copied_at: Arc<AtomicU64>,
}

#[derive(Default)]
struct Bindless {
    /// Start address of the title's shader-visible descriptor memory (0 when
    /// detached or in standalone unit tests without a title heap).
    title_start: u64,
    /// Last observed `(generation << 32 | view_index)` key per title slot (0
    /// if no valid tag), so raw byte copies/overwrites into the title heap
    /// that bypass `mirror()` are detected on bindless sync.
    seen: Vec<u64>,
    /// Last `watch::writes` asking number scanned for byte-copied slots (0 =
    /// initial full scan not yet done).
    scan_asked: u64,
    /// Active `View::is_buffer()` slots: `slot_index -> (view_index, generation)`.
    slots: BTreeMap<u32, (u32, u32)>,
    /// Of them, those viewing placed textures.
    placed_set: BTreeMap<u32, (u32, u32)>,
    /// Ordinary resource-backed CPU-written texture SRV slots (`View::Shader`).
    texture_set: BTreeMap<u32, (u32, u32)>,
    /// Placed-texture and ordinary texture-SRV slots changed since draws last prepared them.
    record_pending: BTreeSet<u32>,
    /// Active 256-byte-aligned bounce slices by `slot_index`.
    cbv_bounce: HashMap<u32, BounceSlot>,
    /// Recycled bounce slices returned when slots are cleared or overwritten.
    bounce_free: Vec<BounceSlot>,
    /// Backing 256 KiB shared buffers `(resource, cpu_ptr, gpu_va, used_bytes)`.
    bounce_chunks: Vec<(ID3D12Resource, usize, u64, u64)>,
    /// Slots written since the plan was made: only they are made again.
    dirty: BTreeSet<u32>,
    /// What each slot made for bindless access needs at submission.
    uses: BTreeMap<u32, SlotUse>,
    /// The released-view, placed-remap, and placed-LRU epochs `uses` was checked at.
    checked: (u64, u64, u64),
    /// What submissions do for the slots, until they or what they view change.
    plan: Option<Arc<BindlessPlan>>,
    /// Placed-texture slots as last prepared at recording: view key,
    /// texture.
    recorded: HashMap<u32, (u64, usize)>,
    /// Textures reached through bindless slots, by resource object: the host
    /// texture (held, so its address names it alone), the frame last uploaded,
    /// and whether it is a placed texture.
    uploaded: HashMap<usize, (ID3D12Resource, u64, bool)>,
    /// Refreshes accepted by the frame scheduler which no submission has
    /// yet delivered. Recording a list does not consume one.
    pending_uploads: HashMap<usize, PendingUpload>,
    /// Uploaded textures by frame, oldest first, to look at again.
    recheck: VecDeque<(u64, RecheckTarget)>,
    /// A frame and the uploads of known textures done in it.
    rechecks: (u64, u32),
    /// When the slot-making budget was last refilled, and the microseconds
    /// left: plans at submission, placed textures at recording.
    budget: Option<(std::time::Instant, [u64; 2])>,
    /// The frame (plus one) released views and evicted textures were last
    /// looked for in: at submission, at recording. Once per frame: the title
    /// releases and evicts all the time, and each look is over all slots.
    looked: [u64; 2],
}

/// A bindless texture queued for periodic CPU-memory recheck.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RecheckTarget {
    Placed(usize),
    /// `(slot_index, view_index, generation, resource_address)`.
    Ordinary(u32, u32, u32, usize),
}

struct PendingUpload {
    request: Arc<crate::texture::RefreshRequest>,
    targets: Vec<RecheckTarget>,
}

enum PendingTex {
    Placed((u32, u32)),
    Ordinary((u32, u32)),
}

fn is_title_texture_srv(view: &View) -> bool {
    matches!(
        view,
        View::Shader(r, _)
            if crate::resource::host_resource(*r as crate::object::This)
                .is_some_and(crate::texture::title_written)
    )
}

/// Time for making changed slots, at submission and at recording: an eighth
/// of the time passing (4 ms of a 30 fps frame), banked up to this many
/// microseconds; what is left over waits.
///
/// shortcut: while the title streams thousands of slots in at once, the
/// slots not made yet read nothing (null views) for a few frames, as
/// streaming pops in; making all of them at once stalled frames for
/// seconds.
const SLOT_BUDGET_US: u64 = 8_000;
/// Slots taken to make at once (the rest wait without being looked at).
const SLOT_CHUNK: usize = 256;

impl Bindless {
    /// The slot-making budget, refilled for the time passed since.
    fn budget(&mut self) -> &mut [u64; 2] {
        let now = std::time::Instant::now();
        let (at, left) = self.budget.get_or_insert((now, [SLOT_BUDGET_US; 2]));
        let add = now.duration_since(*at).as_micros() as u64 / 8;
        if add > 0 {
            *at = now;
            for l in left.iter_mut() {
                *l = (*l + add).min(SLOT_BUDGET_US);
            }
        }
        left
    }
}

/// What one bindless slot needs at submission.
#[derive(Default)]
struct SlotUse {
    /// Title memory it reads (host copy, address, bytes).
    read: Option<(Arc<crate::shadow::Shadow>, usize, usize)>,
    /// What shaders may write through it.
    write: Option<(Arc<crate::shadow::Shadow>, usize, usize)>,
    /// A misaligned constant buffer: bounce slice, title address, bytes,
    /// and its last-copied write-watch asking number.
    bounce: Option<(usize, u64, u32, Arc<AtomicU64>)>,
    /// The host resource it views.
    retained: Option<ID3D12Resource>,
    /// It views a placed texture: the texture's object (0 for none), made
    /// again when the texture is evicted.
    placed: Option<usize>,
    /// It views unmapped memory (looked at again when memory is remapped).
    unmapped: bool,
}

/// Placed textures reached only through bindless slots are uploaded when a
/// slot first reaches them, and then looked at again (their memory hashed)
/// at most this often, in frames, ...
///
/// shortcut: a texture the title rewrites in place without writing a slot
/// shows its new contents up to this many frames late through bindless
/// access (tables still upload what their draws read). Hashing all of them
/// at each submission cost seconds per frame while the world streams in.
const RECHECK_FRAMES: u64 = 60;
/// ... and at most this many of them per frame.
const RECHECKS: u32 = 8;

/// Bumped when views go stale or title memory is mapped or unmapped: what
/// bindless slots view may have changed though no slot was written (their
/// views' generations are checked, all at once, at the next submission).
static VIEWED: AtomicU64 = AtomicU64::new(0);

/// Bindless slots must be checked again: a resource their views name was
/// released, or title memory was mapped or unmapped.
pub(crate) fn bindless_viewed_changed() {
    VIEWED.fetch_add(1, Ordering::Release);
}

/// Bindless unordered access ranges larger than this are not copied back.
///
/// shortcut: copy-back finds what shaders wrote by keeping a range's bytes
/// before each submission; a bindless slot viewing a title's big shared heap
/// would cost a copy and a compare of all of it per submission whether or
/// not a shader writes there. Such ranges stay on the GPU's side.
const WRITE_CAP: usize = 8 << 20;

/// Gap below which two ranges of one host copy are refreshed as one: one
/// 64 KiB write-watch page so adjacent slots share a watch without pulling in
/// unrelated hot memory between sparse historical slots.
const READ_GAP: usize = crate::watch::PAGE;

/// What a submission does for a heap's bindless slots, made again only when
/// the slots or what they view change ([`HostHeap::bindless_plan`]).
pub(crate) struct BindlessPlan {
    /// Title memory the slots read, merged per host copy (address, bytes):
    /// refreshed at each submission using the heap.
    pub(crate) reads: Vec<(Arc<crate::shadow::Shadow>, usize, usize)>,
    /// The unordered access slots' ranges (up to [`WRITE_CAP`] each), merged:
    /// copied back after a submission whose shaders may write memory.
    pub(crate) writes: Vec<(Arc<crate::shadow::Shadow>, usize, usize)>,
    /// Misaligned constant buffers sorted by title address:
    /// `(bounce_cpu, title_address, bytes, last_copied_asking)`.
    bounces: Vec<(usize, u64, u32, Arc<AtomicU64>)>,
    /// The host resources the slots view, held while a submission runs.
    _retained: Vec<ID3D12Resource>,
}

// SAFETY: D3D12 resources are free-threaded; the rest is plain data.
unsafe impl Send for BindlessPlan {}
// SAFETY: as above.
unsafe impl Sync for BindlessPlan {}

impl BindlessPlan {
    /// Copies misaligned constant buffers whose backing pages changed since
    /// their last copy (using write-watch when available, falling back to an
    /// unconditional copy for non-watched memory).
    pub(crate) fn copy_constants(&self) {
        let mut i = 0;
        while i < self.bounces.len() {
            let group_start = self.bounces[i].1 as usize;
            let mut group_end = group_start + self.bounces[i].2 as usize;
            let mut j = i + 1;
            while j < self.bounces.len() {
                let addr = self.bounces[j].1 as usize;
                let end = addr + self.bounces[j].2 as usize;
                if addr <= group_end + crate::watch::PAGE {
                    group_end = group_end.max(end);
                    j += 1;
                } else {
                    break;
                }
            }
            let watch = crate::watch::writes(group_start, group_end, false);
            for (cpu, address, len, copied_at) in &self.bounces[i..j] {
                let (cpu, address, len) = (*cpu, *address, *len);
                if len == 0 {
                    continue;
                }
                if let Some((now, pages)) = &watch {
                    let prev = copied_at.load(Ordering::Relaxed);
                    let first = address as usize / crate::watch::PAGE;
                    let last = (address as usize + len as usize - 1) / crate::watch::PAGE;
                    let dirty = prev == 0
                        || pages
                            .iter()
                            .any(|&(p, w)| (first..=last).contains(&p) && w > prev);
                    if !dirty {
                        continue;
                    }
                    let started = std::time::Instant::now();
                    #[cfg(feature = "diagnostics")]
                    {
                        use crate::copystat::{C, add};
                        add(C::Copies, 1);
                        add(C::CopyBytes, u64::from(len));
                    }
                    crate::host::copy_mapped(
                        cpu as *mut u8,
                        address as *const u8,
                        len as usize,
                        false,
                    );
                    copied_at.store(*now, Ordering::Relaxed);
                    let us = started.elapsed().as_micros() as u64;
                    crate::host::PREPARING[0].fetch_add(us, Ordering::Relaxed);
                    crate::host::PREPARING[1].fetch_add(u64::from(len), Ordering::Relaxed);
                } else {
                    let started = std::time::Instant::now();
                    #[cfg(feature = "diagnostics")]
                    {
                        use crate::copystat::{C, add};
                        add(C::Copies, 1);
                        add(C::CopyBytes, u64::from(len));
                    }
                    crate::host::copy_mapped(
                        cpu as *mut u8,
                        address as *const u8,
                        len as usize,
                        false,
                    );
                    let us = started.elapsed().as_micros() as u64;
                    crate::host::PREPARING[0].fetch_add(us, Ordering::Relaxed);
                    crate::host::PREPARING[1].fetch_add(u64::from(len), Ordering::Relaxed);
                }
            }
            i = j;
        }
    }
}

/// `ranges` (host copy, address, bytes) sorted and merged per host copy
/// where they overlap or lie within `gap` of each other.
fn merged(
    mut ranges: Vec<(Arc<crate::shadow::Shadow>, usize, usize)>,
    gap: usize,
) -> Vec<(Arc<crate::shadow::Shadow>, usize, usize)> {
    ranges.sort_unstable_by_key(|(s, a, _)| (Arc::as_ptr(s) as usize, *a));
    let mut out: Vec<(Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for (shadow, address, len) in ranges {
        if let Some((s, a, l)) = out.last_mut()
            && Arc::ptr_eq(s, &shadow)
            && address <= *a + *l + gap
        {
            *l = (*l).max(address + len - *a);
            continue;
        }
        out.push((shadow, address, len));
    }
    out
}

/// Host heaps' GPU ranges (start, bytes, bytes per slot, sampler), for
/// device-removed reports: a shader indexing a heap past its end faults.
static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

/// For device-removed reports: where host GPU address `va` lies against
/// the host descriptor heaps.
pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!(
                "{:#x} past its end (slot {})",
                va - start - bytes,
                (va - start) / step
            )
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2; tier 3 hardware may take more.
const PORTABLE_VIEWS: u32 = 1_000_000;

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw). A list holds the slots it
/// took until it is reset (its submissions have run) or released.
/// 2^16 ran out seconds into the world (every world run on an RX 9070 XT,
/// each a device removal shortly after; #59).
const DYNAMIC: u32 = 1 << 18;

/// The dynamic slots of a heap whose host would not take [`DYNAMIC`] more
/// (a capped heap, NVIDIA): the title's slots keep their room first.
const MIN_DYNAMIC: u32 = 1 << 16;

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32, title_start: u64) -> Option<Self> {
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        // The larger dynamic range only where the host takes it on top of
        // every title slot; else the smaller one, and the cap after that.
        let (heap, count, extra) = if sampler {
            match make(count) {
                Ok(heap) => (heap, count, 0),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else if let Ok(heap) = make(count + DYNAMIC) {
            (heap, count, DYNAMIC)
        } else {
            match make(count + MIN_DYNAMIC) {
                Ok(heap) => (heap, count, MIN_DYNAMIC),
                Err(e) if count + MIN_DYNAMIC > PORTABLE_VIEWS => {
                    // shortcut: descriptors past the host's limit are left
                    // out; a title that reaches them needs remapping.
                    tracing::warn!(error = %e, count, "host view heap capped at {PORTABLE_VIEWS}");
                    (make(PORTABLE_VIEWS).ok()?, PORTABLE_VIEWS - MIN_DYNAMIC, MIN_DYNAMIC)
                }
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        };
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        // SAFETY: a live shader-visible heap.
        let gpu = unsafe { heap.GetGPUDescriptorHandleForHeapStart().ptr };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(count + extra) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let h = HostHeap {
            heap,
            cpu,
            step,
            count,
            dynamic: extra,
            sampler,
            made: Mutex::new(vec![0; count as usize]),
            kept: Mutex::new(vec![None; (count + extra) as usize]),
            next: Mutex::new((0, vec![false; extra as usize])),
            bindless: Mutex::new(Bindless {
                title_start: if sampler { 0 } else { title_start },
                seen: if sampler {
                    Vec::new()
                } else {
                    vec![0; count as usize]
                },
                ..Bindless::default()
            }),
            recording: AtomicU64::new(0),
            record_hint: std::sync::atomic::AtomicBool::new(false),
            record_looked: [AtomicU64::new(0), AtomicU64::new(0)],
            recheck_at: AtomicU64::new(u64::MAX),
        };
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot starts
        // null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, count + extra) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(
            ms = started.elapsed().as_millis(),
            "host descriptor heap made null"
        );
        Some(h)
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: BLOCK,
                        ..Default::default()
                    })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                null_sampler(start);
            } else {
                null_view(start);
            }
            let mut filled = 1;
            while filled < BLOCK {
                let n = filled.min(BLOCK - filled);
                host.device
                    .CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            let mut done = 0;
            while done < total {
                let n = BLOCK.min(total - done);
                host.device
                    .CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                done += n;
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    fn null_sampler(&self, index: u32) {
        null_sampler(self.cpu(index));
    }
}

/// A plain point sampler at `at`.
fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

impl HostHeap {
    /// The microseconds of slot making (`which`: 0 plans, 1 recording) left.
    fn budget_left(&self, which: usize) -> u64 {
        self.bindless
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .budget()[which]
    }

    /// Notes `us` microseconds of slot making spent.
    fn budget_spend(&self, which: usize, us: u64) {
        let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
        let left = &mut b.budget()[which];
        *left = left.saturating_sub(us);
    }

    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release`]: the first's index.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (next, taken) = &mut *guard;
        let n = n as usize;
        // Search from `next` on, then from the start.
        let starts = (*next as usize..self.dynamic as usize).chain(0..*next as usize);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts {
            // A run cannot wrap past the end.
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        let Some(first) = found else {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(n, "no free dynamic descriptor slots: a table is not made")
            });
            return None;
        };
        taken[first..first + n].fill(true);
        *next = ((first + n) % self.dynamic as usize) as u32;
        Some(self.count + first as u32)
    }

    /// The title slots this heap has host slots for; fewer than the title's
    /// when the host caps its size.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned).
    ///
    /// [`dynamic`]: HostHeap::dynamic
    pub(crate) fn release(&self, first: u32, n: u32) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let end = (at + n).min(self.dynamic) as usize;
        if let Some(slots) = guard.1.get_mut(at as usize..end) {
            slots.fill(false);
        }
    }

    /// Makes slot `index` (a dynamic one) from `view`; a null view if the
    /// host cannot.
    pub(crate) fn make_at(&self, index: u32, view: &View) {
        let kept = make(view, self.cpu(index));
        if kept.is_none() {
            null_for(view, self.cpu(index));
        }
        self.keep(index, kept.flatten());
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.cpu(index));
        }
        self.keep(index, None);
    }

    /// Slot `index` now views `resource` (or nothing of the host's).
    fn keep(&self, index: u32, resource: Option<ID3D12Resource>) {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = kept.get_mut(index as usize) {
            *slot = resource;
        }
    }

    /// Makes slot `index` (a dynamic one) a constant buffer view of host
    /// memory (the list's own, which it holds).
    pub(crate) fn constant_at(&self, index: u32, gpu: u64, size: u32) {
        self.keep(index, None);
        let Some(host) = host() else { return };
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: size.min(65_536).next_multiple_of(256),
        };
        // SAFETY: a slot of our heap; the memory is the caller's.
        unsafe {
            host.device
                .CreateConstantBufferView(Some(&desc), self.cpu(index))
        };
    }

    fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there. Buffer
    /// views are made only for draws (`for_draw`), which also copy their
    /// data: a host copy of a buffer is made on first use.
    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        if index >= self.count {
            // Diagnostic (NVIDIA heap cap): the highest title slot past the
            // host heap seen so far.
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, Ordering::Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
        let Some(view) = view else {
            // Not a view of ours (never written, or of a resource the
            // title released): nothing to read.
            {
                let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
                if made[index as usize] != 0 {
                    made[index as usize] = 0;
                    self.make_null(index);
                }
            }
            if !self.sampler {
                self.untrack_bindless(index, 0);
            }
            return None;
        };
        let is_buf = !self.sampler && view.is_buffer();
        {
            let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            if made[index as usize] != key {
                let later = !for_draw && is_buf;
                made[index as usize] = if later { 0 } else { key };
                let kept = if later {
                    None
                } else {
                    make(&view, self.cpu(index))
                };
                if kept.is_none() && !self.sampler {
                    null_for(&view, self.cpu(index));
                }
                self.keep(index, kept.flatten());
            }
        }
        if !self.sampler {
            if is_buf {
                let placed = matches!(&view, View::Placed(false, r, _) if r[0] != 1);
                self.track_bindless(index, (i, generation), placed);
            } else if is_title_texture_srv(&view) {
                self.track_ordinary_texture(index, (i, generation), key);
            } else {
                self.untrack_bindless(index, key);
            }
        }
        Some(view)
    }

    /// Detaches the title's descriptor memory when its `Heap` is dropped so
    /// later bindless scans never read freed heap memory.
    pub(crate) fn detach_title(&self) {
        let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
        b.title_start = 0;
    }

    fn recycle_bounce(b: &mut Bindless, index: u32) {
        if let Some(slot) = b.cbv_bounce.remove(&index) {
            slot.copied_at.store(0, Ordering::Relaxed);
            b.bounce_free.push(slot);
        }
    }

    fn track_bindless(&self, index: u32, entry: (u32, u32), placed: bool) {
        let key = u64::from(entry.1) << 32 | u64::from(entry.0);
        let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(seen) = b.seen.get_mut(index as usize) {
            *seen = key;
        }
        if b.slots.insert(index, entry) != Some(entry) {
            b.dirty.insert(index);
            Self::recycle_bounce(&mut b, index);
        }
        let changed = if placed {
            b.placed_set.insert(index, entry) != Some(entry)
        } else {
            b.placed_set.remove(&index).is_some()
        };
        b.texture_set.remove(&index);
        if changed {
            b.record_pending.insert(index);
            self.record_hint.store(true, Ordering::Release);
        }
    }

    fn track_ordinary_texture(&self, index: u32, entry: (u32, u32), seen_key: u64) {
        let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(seen) = b.seen.get_mut(index as usize) {
            *seen = seen_key;
        }
        if b.slots.remove(&index).is_some() {
            b.dirty.insert(index);
            Self::recycle_bounce(&mut b, index);
        }
        if b.placed_set.remove(&index).is_some() {
            b.recorded.remove(&index);
        }
        if b.texture_set.insert(index, entry) != Some(entry) {
            b.record_pending.insert(index);
            self.record_hint.store(true, Ordering::Release);
        }
    }

    fn untrack_bindless(&self, index: u32, seen_key: u64) {
        let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(seen) = b.seen.get_mut(index as usize) {
            *seen = seen_key;
        }
        if b.slots.remove(&index).is_some() {
            b.dirty.insert(index);
            Self::recycle_bounce(&mut b, index);
        }
        if b.placed_set.remove(&index).is_some() {
            b.recorded.remove(&index);
        }
        b.texture_set.remove(&index);
    }

    /// Scans title descriptor memory for slots written or overwritten as raw
    /// bytes (bypassing `mirror()`), using write-watch when the heap's memory
    /// supports it and scanning all slots otherwise.
    fn sync_title_slots(&self) {
        if self.sampler || self.count == 0 {
            return;
        }
        let start = {
            let b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
            b.title_start
        };
        if start == 0 {
            return;
        }
        const TITLE_SLOT_BYTES: usize = 32;
        let len = self.count as usize * TITLE_SLOT_BYTES;
        let end = start as usize + len;
        let watch = crate::watch::writes(start as usize, end, false);

        let mut cleared = Vec::new();
        let mut direct = Vec::new();
        {
            let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
            if b.title_start != start {
                return;
            }
            let b = &mut *b;
            let prev_asked = b.scan_asked;
            let mut check_range = |first: u32, last: u32| {
                for index in first..last {
                    let handle = start + u64::from(index) * TITLE_SLOT_BYTES as u64;
                    // SAFETY: `index < self.count` lies in the live title heap
                    // (`detach_title` clears `b.title_start` under this lock).
                    let [tag, vi, generation] =
                        unsafe { (handle as *const [u32; 3]).read_unaligned() };
                    let key = if tag == VIEW_TAG && generation != 0 {
                        u64::from(generation) << 32 | u64::from(vi)
                    } else {
                        0
                    };
                    let Some(seen) = b.seen.get_mut(index as usize) else {
                        continue;
                    };
                    if *seen == key {
                        continue;
                    }
                    *seen = key;
                    if key == 0 {
                        if b.slots.remove(&index).is_some() {
                            b.dirty.insert(index);
                            Self::recycle_bounce(b, index);
                        }
                        if b.placed_set.remove(&index).is_some() {
                            b.recorded.remove(&index);
                        }
                        b.texture_set.remove(&index);
                        cleared.push(index);
                        continue;
                    }
                    let Some(view) = view_at(vi, generation) else {
                        if b.slots.remove(&index).is_some() {
                            b.dirty.insert(index);
                            Self::recycle_bounce(b, index);
                        }
                        if b.placed_set.remove(&index).is_some() {
                            b.recorded.remove(&index);
                        }
                        b.texture_set.remove(&index);
                        cleared.push(index);
                        continue;
                    };
                    if view.is_buffer() {
                        let entry = (vi, generation);
                        if b.slots.insert(index, entry) != Some(entry) {
                            b.dirty.insert(index);
                            Self::recycle_bounce(b, index);
                        }
                        let placed = matches!(&view, View::Placed(false, r, _) if r[0] != 1);
                        let changed = if placed {
                            b.placed_set.insert(index, entry) != Some(entry)
                        } else {
                            b.placed_set.remove(&index).is_some()
                        };
                        b.texture_set.remove(&index);
                        if changed {
                            b.record_pending.insert(index);
                            self.record_hint.store(true, Ordering::Release);
                        }
                    } else {
                        if b.slots.remove(&index).is_some() {
                            b.dirty.insert(index);
                            Self::recycle_bounce(b, index);
                        }
                        if b.placed_set.remove(&index).is_some() {
                            b.recorded.remove(&index);
                        }
                        if is_title_texture_srv(&view) {
                            let entry = (vi, generation);
                            if b.texture_set.insert(index, entry) != Some(entry) {
                                b.record_pending.insert(index);
                                self.record_hint.store(true, Ordering::Release);
                            }
                        } else {
                            b.texture_set.remove(&index);
                        }
                        direct.push((index, key, view));
                    }
                }
            };

            match watch {
                Some((now, pages)) if prev_asked != 0 => {
                    let base = start as usize;
                    for (page, written_at) in pages {
                        if written_at <= prev_asked {
                            continue;
                        }
                        let page_start = page * crate::watch::PAGE;
                        let page_end = page_start + crate::watch::PAGE;
                        let lo = page_start.saturating_sub(base) / TITLE_SLOT_BYTES;
                        let hi = page_end
                            .saturating_sub(base)
                            .div_ceil(TITLE_SLOT_BYTES)
                            .min(self.count as usize);
                        if lo < hi {
                            check_range(lo as u32, hi as u32);
                        }
                    }
                    b.scan_asked = b.scan_asked.max(now);
                }
                Some((now, _)) => {
                    check_range(0, self.count);
                    b.scan_asked = b.scan_asked.max(now);
                }
                None => {
                    check_range(0, self.count);
                }
            }
        }
        for index in cleared {
            self.clear_stale_slot(index);
        }
        for (index, key, view) in direct {
            let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            if made[index as usize] != key {
                let kept = make(&view, self.cpu(index));
                if kept.is_none() {
                    null_for(&view, self.cpu(index));
                    made[index as usize] = 0;
                } else {
                    made[index as usize] = key;
                }
                self.keep(index, kept.flatten());
            }
        }
    }

    /// Slot `index` (viewing `entry`) is no longer one for bindless access.
    fn drop_bindless(&self, b: &mut Bindless, index: u32, entry: Option<(u32, u32)>) {
        b.uses.remove(&index);
        b.recorded.remove(&index);
        if entry.is_none_or(|e| b.slots.get(&index) == Some(&e)) {
            b.slots.remove(&index);
            Self::recycle_bounce(b, index);
            b.placed_set.remove(&index);
        }
        if entry.is_none_or(|e| b.texture_set.get(&index) == Some(&e)) {
            b.texture_set.remove(&index);
        }
    }

    fn clear_stale_slot(&self, index: u32) {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if made[index as usize] != 0 {
            made[index as usize] = 0;
            self.make_null(index);
        }
    }

    fn set_unmapped_null(&self, index: u32, view: &View) {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        made[index as usize] = 0;
        if !self.sampler {
            null_for(view, self.cpu(index));
        }
        self.keep(index, None);
    }

    fn ensure_placed_slot(
        &self,
        index: u32,
        key: u64,
        shader_view: &View,
        raw_tex: *mut std::ffi::c_void,
    ) {
        use windows::core::Interface;
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let same_tex = self
            .kept
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)
            .and_then(Option::as_ref)
            .is_some_and(|r| r.as_raw() == raw_tex);
        if made[index as usize] != key || !same_tex {
            let kept = make(shader_view, self.cpu(index));
            if kept.is_none() {
                null_for(shader_view, self.cpu(index));
                made[index as usize] = 0;
            } else {
                made[index as usize] = key;
            }
            self.keep(index, kept.flatten());
        }
    }

    fn ensure_buffer_slot(&self, index: u32, key: u64, view: &View) -> Option<ID3D12Resource> {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if made[index as usize] != key {
            let kept = make(view, self.cpu(index));
            let res = kept.flatten();
            if res.is_none() {
                null_for(view, self.cpu(index));
                made[index as usize] = 0;
            } else {
                made[index as usize] = key;
            }
            self.keep(index, res.clone());
            return res;
        }
        self.kept
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)
            .and_then(Option::clone)
    }

    fn ensure_cbv_bounce(
        &self,
        index: u32,
        aligned: u32,
    ) -> Option<(u64, usize, ID3D12Resource, Arc<AtomicU64>, bool)> {
        let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = b.cbv_bounce.get(&index)
            && slot.cap >= aligned
        {
            return Some((
                slot.gpu,
                slot.cpu,
                slot.resource.clone(),
                slot.copied_at.clone(),
                true,
            ));
        }
        Self::recycle_bounce(&mut b, index);
        if let Some(pos) = b.bounce_free.iter().position(|s| s.cap >= aligned) {
            let slot = b.bounce_free.swap_remove(pos);
            slot.copied_at.store(0, Ordering::Relaxed);
            let out = (
                slot.gpu,
                slot.cpu,
                slot.resource.clone(),
                slot.copied_at.clone(),
                false,
            );
            b.cbv_bounce.insert(index, slot);
            return Some(out);
        }
        let needed = u64::from(aligned);
        let need_chunk = b
            .bounce_chunks
            .last()
            .is_none_or(|&(_, _, _, used)| used + needed > BOUNCE_CHUNK);
        if need_chunk {
            let chunk_size = BOUNCE_CHUNK.max(needed);
            let (res, data) = host()?.shared_buffer(chunk_size)?;
            crate::host::name(&res, || "bindless cbv bounce".into());
            // SAFETY: a live buffer.
            let gpu = unsafe { res.GetGPUVirtualAddress() };
            b.bounce_chunks.push((res, data as usize, gpu, 0));
        }
        let last = b.bounce_chunks.last_mut()?;
        let at = last.3;
        last.3 += needed;
        let (gpu, cpu, res) = (last.2 + at, last.1 + at as usize, last.0.clone());
        let copied_at = Arc::new(AtomicU64::new(0));
        b.cbv_bounce.insert(
            index,
            BounceSlot {
                gpu,
                cpu,
                cap: aligned,
                resource: res.clone(),
                copied_at: copied_at.clone(),
            },
        );
        Some((gpu, cpu, res, copied_at, false))
    }

    fn materialize_bindless_cbv(
        &self,
        index: u32,
        key: u64,
        address: u64,
        capped: u32,
        reads: &mut Vec<(Arc<crate::shadow::Shadow>, usize, usize)>,
        bounces: &mut Vec<(usize, u64, u32, Arc<AtomicU64>)>,
    ) -> Option<ID3D12Resource> {
        let host = host()?;
        let aligned = capped.next_multiple_of(256);
        if address.is_multiple_of(256)
            && let Some(shadow) = crate::shadow::find(address, u64::from(capped))
            && shadow.gpu(address).is_multiple_of(256)
        {
            let had_bounce = {
                let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
                let had = b.cbv_bounce.contains_key(&index);
                if had {
                    Self::recycle_bounce(&mut b, index);
                }
                had
            };
            {
                let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
                if made[index as usize] != key || had_bounce {
                    let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                        BufferLocation: shadow.gpu(address),
                        SizeInBytes: aligned,
                    };
                    // SAFETY: a slot of our heap; `shadow` covers the range.
                    unsafe {
                        host.device
                            .CreateConstantBufferView(Some(&desc), self.cpu(index))
                    };
                    made[index as usize] = key;
                    self.keep(index, Some(shadow.buffer.clone()));
                }
            }
            let len = u64::from(capped).min(shadow.left(address)) as usize;
            if len > 0 {
                reads.push((shadow.clone(), address as usize, len));
            }
            return Some(shadow.buffer.clone());
        }

        let (gpu, cpu, res, copied_at, was_active) = self.ensure_cbv_bounce(index, aligned)?;
        {
            let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            if made[index as usize] != key || !was_active {
                let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                    BufferLocation: gpu,
                    SizeInBytes: aligned,
                };
                // SAFETY: a slot of our heap; `gpu` is 256-byte aligned in `res`.
                unsafe {
                    host.device
                        .CreateConstantBufferView(Some(&desc), self.cpu(index))
                };
                made[index as usize] = key;
                self.keep(index, Some(res.clone()));
            }
        }
        bounces.push((cpu, address, capped, copied_at));
        Some(res)
    }

    /// At draw/dispatch recording time: makes the placed-texture slots
    /// changed since (or whose textures were evicted), and schedules refreshes
    /// for new textures and known ones due a look ([`RECHECK_FRAMES`]). Move
    /// the textures to the shader resource state on this list. Buffer and
    /// constant-buffer views are prepared from [`HostHeap::bindless_plan`].
    ///
    /// A pending CPU refresh can be delivered by a different submitted
    /// list. State changes recorded here describe this list's normal uses.
    pub(crate) fn prepare_bindless_record(&self, list: &crate::host::HostList) {
        if self.sampler {
            return;
        }
        let epoch = crate::resource::placed_epoch();
        let frame = crate::queue::presented();
        // Most draws find nothing to do: a few atomics, no lock.
        let evictions = self.record_looked[0].load(Ordering::Acquire) != epoch
            && self.record_looked[1].load(Ordering::Acquire) != frame + 1;
        if !evictions
            && !self.record_hint.load(Ordering::Acquire)
            && self.recheck_at.load(Ordering::Acquire) > frame
        {
            return;
        }
        // One recording thread prepares at a time; the others go on.
        let revision = self.recording.load(Ordering::Acquire);
        if revision & 1 != 0
            || self.recording.compare_exchange(
                revision, revision.wrapping_add(1), Ordering::Acquire, Ordering::Relaxed,
            ).is_err()
        {
            return;
        }
        struct Done<'a>(&'a AtomicU64);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Release);
            }
        }
        let _done = Done(&self.recording);
        let started = std::time::Instant::now();
        // Asked before taking the heap's lock: the placed textures' lock is
        // taken first where both are held.
        let live = evictions.then(crate::resource::placed_live);
        let (todo, due) = {
            // The submission may hold the heap's lock for a while: the work
            // waits for a later draw rather than this one waiting.
            let mut b = match self.bindless.try_lock() {
                Ok(b) => b,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return,
            };
            let b = &mut *b;
            if let Some(live) = &live {
                b.uploaded
                    .retain(|t, (_, _, placed)| !*placed || live.contains(t));
                for (&slot, &(_, t)) in &b.recorded {
                    if !live.contains(&t) {
                        b.record_pending.insert(slot);
                    }
                }
                b.recorded.retain(|_, (_, t)| live.contains(t));
                self.record_looked[0].store(epoch, Ordering::Release);
                self.record_looked[1].store(frame + 1, Ordering::Release);
            }
            let mut todo: Vec<(u32, PendingTex)> = Vec::new();
            if b.budget()[1] > 0 {
                while todo.len() < SLOT_CHUNK
                    && let Some(i) = b.record_pending.pop_first()
                {
                    if let Some(entry) = b.placed_set.get(&i).copied() {
                        todo.push((i, PendingTex::Placed(entry)));
                    } else if let Some(entry) = b.texture_set.get(&i).copied() {
                        todo.push((i, PendingTex::Ordinary(entry)));
                    }
                }
            }
            let mut due = Vec::new();
            while due.len() < RECHECKS as usize
                && (b.rechecks.0 != frame || b.rechecks.1 < RECHECKS)
                && let Some(&(f, target)) = b.recheck.front()
                && f + RECHECK_FRAMES <= frame
            {
                b.recheck.pop_front();
                due.push((f, target));
            }
            self.recheck_at.store(
                b.recheck
                    .front()
                    .map_or(u64::MAX, |&(f, _)| f + RECHECK_FRAMES),
                Ordering::Release,
            );
            self.record_hint
                .store(!b.record_pending.is_empty(), Ordering::Release);
            (todo, due)
        };
        if todo.is_empty() && due.is_empty() {
            return;
        }
        let mut stale = Vec::new();
        let mut stale_ordinary: Vec<(usize, u64)> = Vec::new();
        let mut learnt = Vec::new();
        let mut reached = Vec::new();
        let mut reached_ordinary = Vec::new();
        let left = self.budget_left(1);
        let mut todo = todo.into_iter();
        let mut over = Vec::new();
        for (index, item) in todo.by_ref() {
            if started.elapsed().as_micros() as u64 > left {
                // Over budget: the rest waits (this one too).
                over.push(index);
                break;
            }
            match item {
                PendingTex::Placed((vi, generation)) => {
                    match self.resolve_placed(index, vi, generation) {
                        Ok(Some(held)) => {
                            let key = u64::from(generation) << 32 | u64::from(vi);
                            learnt.push((index, (key, held.as_this() as usize)));
                            reached.push(held);
                        }
                        Ok(None) => {}
                        Err(()) => stale.push(index),
                    }
                }
                PendingTex::Ordinary((vi, generation)) => {
                    match self.resolve_ordinary_texture(index, vi, generation) {
                        Ok(Some(tex)) => reached_ordinary.push((index, vi, generation, tex)),
                        Ok(None) => {}
                        Err(()) => stale.push(index),
                    }
                }
            }
        }
        over.extend(todo.map(|(i, _)| i));
        for (recheck_frame, target) in due {
            match target {
                RecheckTarget::Placed(t) => {
                    if let Some(held) = crate::resource::hold_placed(t as crate::object::This) {
                        reached.push(held);
                    }
                }
                RecheckTarget::Ordinary(slot, vi, generation, tex_addr) => {
                    match self.resolve_ordinary_texture(slot, vi, generation) {
                        Ok(Some(tex)) => reached_ordinary.push((slot, vi, generation, tex)),
                        Ok(None) => {}
                        Err(()) => {
                            stale.push(slot);
                            stale_ordinary.push((tex_addr, recheck_frame));
                        }
                    }
                }
            }
        }
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        for held in reached {
            let texture = held.as_this();
            let Some(host_tex) = crate::resource::host_resource(texture) else {
                continue;
            };
            if crate::texture::title_written(host_tex) {
                self.queue_texture_refresh(
                    texture,
                    host_tex,
                    frame,
                    RecheckTarget::Placed(texture as usize),
                );
            }
            crate::list::ensure_state(list, texture, SHADER_RESOURCE);
        }
        for (slot, vi, generation, texture) in reached_ordinary {
            let Some(host_tex) = crate::resource::host_resource(texture) else {
                continue;
            };
            if crate::texture::title_written(host_tex) {
                self.queue_texture_refresh(
                    texture,
                    host_tex,
                    frame,
                    RecheckTarget::Ordinary(slot, vi, generation, texture as usize),
                );
            }
            crate::list::ensure_state(list, texture, SHADER_RESOURCE);
        }
        {
            let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
            b.recorded.extend(learnt);
            for idx in stale {
                self.drop_bindless(&mut b, idx, None);
            }
            for (tex_addr, recheck_frame) in stale_ordinary {
                if b.uploaded
                    .get(&tex_addr)
                    .is_some_and(|&(_, last, placed)| !placed && last <= recheck_frame)
                {
                    b.uploaded.remove(&tex_addr);
                }
            }
            b.record_pending.extend(over);
            self.record_hint
                .store(!b.record_pending.is_empty(), Ordering::Release);
        }
        let us = started.elapsed().as_micros() as u64;
        self.budget_spend(1, us);
        crate::host::BINDLESS_US[1].fetch_add(us, Ordering::Relaxed);
    }

    /// The placed texture slot `index` views, made in the slot: `Err` if the
    /// view went stale, `None` if it is not a placed texture or cannot be made
    /// (the slot is then null). Returns a held reference so eviction cannot
    /// free it before the caller finishes uploading/transitioning.
    fn resolve_placed(
        &self,
        index: u32,
        vi: u32,
        generation: u32,
    ) -> Result<Option<crate::resource::HeldTexture>, ()> {
        use windows::core::Interface;
        let Some(view) = view_at(vi, generation) else {
            self.clear_stale_slot(index);
            return Err(());
        };
        let View::Placed(false, resource, desc) = &view else {
            return Ok(None);
        };
        const BUFFER: u32 = 1;
        if resource[0] == BUFFER {
            return Ok(None);
        }
        let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
        if !valid_title_address(address, 1) {
            self.set_unmapped_null(index, &view);
            return Ok(None);
        }
        let Some((shader_view, held)) = placed_texture(&view) else {
            self.set_unmapped_null(index, &view);
            return Ok(None);
        };
        let tex_this = held.as_this();
        let Some(host_tex) = crate::resource::host_resource(tex_this) else {
            self.set_unmapped_null(index, &view);
            return Ok(None);
        };
        let key = u64::from(generation) << 32 | u64::from(vi);
        self.ensure_placed_slot(index, key, &shader_view, host_tex.as_raw());
        Ok(Some(held))
    }

    /// Resolves an ordinary resource-backed texture SRV at slot `index`:
    /// `Err` if the view went stale (resource released), `None` if it is no
    /// longer a texture SRV.
    fn resolve_ordinary_texture(
        &self,
        index: u32,
        vi: u32,
        generation: u32,
    ) -> Result<Option<crate::object::This>, ()> {
        let Some(view) = view_at(vi, generation) else {
            self.clear_stale_slot(index);
            return Err(());
        };
        let View::Shader(r, _) = view else {
            return Ok(None);
        };
        let tex = r as crate::object::This;
        if crate::resource::host_resource(tex).is_none() {
            return Ok(None);
        }
        Ok(Some(tex))
    }

    fn queue_texture_refresh(
        &self,
        texture: crate::object::This,
        host: &ID3D12Resource,
        frame: u64,
        target: RecheckTarget,
    ) {
        use windows::core::Interface;
        let identity = texture as usize;
        let due = self.upload_due(texture, host, frame, target);
        let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(queued) = b.pending_uploads.get_mut(&identity)
            && queued.request.copy.texture.as_raw() == host.as_raw()
        {
            if !queued.targets.contains(&target) {
                queued.targets.push(target);
            }
            return;
        }
        if due && let Some(request) = crate::texture::refresh_request(texture, host) {
            b.pending_uploads.insert(identity, PendingUpload {
                request: Arc::new(request),
                targets: vec![target],
            });
        }
    }

    /// Snapshots only tickets still reachable through a current title slot
    /// and the same native texture. Check the raw record too, since the
    /// title can overwrite descriptor memory without calling mirror().
    pub(crate) fn pending_texture_refreshes(
        &self,
    ) -> Vec<(usize, Arc<crate::texture::RefreshRequest>)> {
        use windows::core::Interface;
        let (start, revision, candidates) = {
            let b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
            if b.pending_uploads.is_empty() {
                return Vec::new();
            }
            let mut placed_slots: HashMap<usize, Vec<(u32, u32, u32)>> = HashMap::new();
            if b.pending_uploads.values().any(|pending| pending.targets.iter()
                .any(|target| matches!(target, RecheckTarget::Placed(_))))
            {
                for (&slot, &(key, texture)) in &b.recorded {
                    let pair = (key as u32, (key >> 32) as u32);
                    if b.pending_uploads.contains_key(&texture)
                        && b.placed_set.get(&slot) == Some(&pair)
                    {
                        placed_slots.entry(texture).or_default().push((slot, pair.0, pair.1));
                    }
                }
            }
            let candidates = b.pending_uploads.iter().map(|(&identity, pending)| {
                let ordinary = pending.targets.iter().filter_map(|&target| match target {
                    RecheckTarget::Ordinary(slot, vi, generation, _)
                        if b.texture_set.get(&slot) == Some(&(vi, generation)) =>
                    {
                        Some((slot, vi, generation))
                    }
                    _ => None,
                }).collect::<Vec<_>>();
                (identity, pending.request.clone(), ordinary, placed_slots.remove(&identity))
            }).collect::<Vec<_>>();
            (b.title_start, self.recording.load(Ordering::Acquire), candidates)
        };
        let mut live = Vec::new();
        for (identity, request, ordinary, placed) in candidates {
            let placed_live = placed.as_ref()
                .and_then(|_| crate::resource::hold_placed(identity as crate::object::This));
            let slots = ordinary.into_iter().chain(placed.into_iter()
                .filter(|_| placed_live.is_some()).flatten());
            let matches = |(slot, vi, generation)| {
                let mut record = [0u8; 12];
                let address = start + u64::from(slot) * 32;
                if !crate::host::read_into(address as *const u8, &mut record) {
                    return false;
                }
                let word = |i: usize| u32::from_le_bytes(record[i * 4..i * 4 + 4].try_into().unwrap());
                word(0) == VIEW_TAG && word(1) == vi && word(2) == generation
                    && view_at(vi, generation).is_some()
                    && self.kept.lock().unwrap_or_else(|e| e.into_inner())
                        .get(slot as usize).and_then(Option::as_ref)
                        .is_some_and(|native| native.as_raw() == request.copy.texture.as_raw())
            };
            if start != 0 && slots.into_iter().any(matches) {
                live.push((identity, request));
            } else if revision & 1 == 0 {
                self.remove_pending_refresh(identity, &request, Some(revision));
            }
        }
        live
    }

    /// A newer ticket may have replaced this address while a submission was
    /// in flight. Only retire the request that was validated for that list.
    pub(crate) fn remove_pending_refresh(
        &self,
        identity: usize,
        request: &Arc<crate::texture::RefreshRequest>,
        stale_revision: Option<u64>,
    ) {
        use windows::core::Interface;
        let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
        if stale_revision.is_some_and(|revision| self.recording.load(Ordering::Acquire) != revision) {
            return;
        }
        if b.pending_uploads.get(&identity).is_some_and(|queued| Arc::ptr_eq(&queued.request, request)) {
            b.pending_uploads.remove(&identity);
            if stale_revision.is_some() && b.uploaded.get(&identity).is_some_and(|entry|
                entry.0.as_raw() == request.copy.texture.as_raw())
            {
                b.uploaded.remove(&identity);
            }
        }
    }

    /// Whether to upload bindless texture `texture` (reached through a slot)
    /// in a list recorded in `frame`: the first time, then as
    /// [`RECHECK_FRAMES`] allows.
    fn upload_due(
        &self,
        texture: crate::object::This,
        host: &ID3D12Resource,
        frame: u64,
        target: RecheckTarget,
    ) -> bool {
        use windows::core::Interface;
        let is_placed = matches!(target, RecheckTarget::Placed(_));
        let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
        let b = &mut *b;
        let due = match b.uploaded.get(&(texture as usize)) {
            None => true,
            // Another texture at the object's address.
            Some((h, _, _)) if h.as_raw() != host.as_raw() => true,
            Some((_, last, _)) if frame >= last + RECHECK_FRAMES => {
                if b.rechecks.0 != frame {
                    b.rechecks = (frame, 0);
                }
                b.rechecks.1 += 1;
                b.rechecks.1 <= RECHECKS
            }
            Some(_) => false,
        };
        if due {
            b.uploaded
                .insert(texture as usize, (host.clone(), frame, is_placed));
            b.recheck.push_back((frame, target));
            self.recheck_at.store(
                b.recheck
                    .front()
                    .map_or(u64::MAX, |&(f, _)| f + RECHECK_FRAMES),
                Ordering::Release,
            );
        }
        due
    }

    /// At submission: what the heap's bindless slots need. Only slots
    /// written since the last plan are made again; released views are found
    /// by one look at all slots' generations, and slots of evicted placed
    /// textures (or unmapped memory, once remapped) are made again. With
    /// nothing changed it is the last plan, whose reads the caller
    /// refreshes, which costs little when the title changed nothing.
    pub(crate) fn bindless_plan(&self) -> Option<Arc<BindlessPlan>> {
        if self.sampler {
            return None;
        }
        self.sync_title_slots();
        let epochs = (
            VIEWED.load(Ordering::Acquire),
            crate::resource::placed_epoch(),
            crate::resource::placed_lru_epoch(),
        );
        // Asked before taking the heap's lock: the placed textures' lock is
        // taken first where both are held.
        let frame = crate::queue::presented() + 1;
        let evictions = {
            let b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
            (b.checked.1 != epochs.1 || b.checked.2 != epochs.2) && b.looked[0] != frame
        };
        let live = evictions.then(crate::resource::placed_live);
        let (mut todo, check, lru_cleared) = {
            let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
            let b = &mut *b;
            // Released views and evicted textures are looked for once a frame.
            let look = b.looked[0] != frame && b.checked != epochs;
            if b.dirty.is_empty()
                && !look
                && let Some(plan) = &b.plan
            {
                return Some(plan.clone());
            }
            if !look
                && b.budget()[0] == 0
                && let Some(plan) = &b.plan
            {
                return Some(plan.clone());
            }
            let viewed = look && b.checked.0 != epochs.0;
            let remapped = look && b.checked.1 != epochs.1;
            let lru_trimmed = look && b.checked.2 != epochs.2;
            let live = if look { live.as_ref() } else { None };
            if look {
                b.checked = epochs;
                b.looked[0] = frame;
            }
            let mut dirty = BTreeSet::new();
            while dirty.len() < 4 * SLOT_CHUNK
                && let Some(i) = b.dirty.pop_first()
            {
                dirty.insert(i);
            }
            let mut lru_cleared = Vec::new();
            for (&i, u) in &mut b.uses {
                let evicted = live
                    .zip(u.placed)
                    .is_some_and(|(l, t)| t != 0 && !l.contains(&t));
                if evicted && !remapped && lru_trimmed {
                    // LRU-trimmed texture: release retained ref and null out
                    // the host slot without re-allocating it.
                    u.placed = None;
                    u.retained = None;
                    lru_cleared.push(i);
                } else if evicted || viewed && u.unmapped {
                    dirty.insert(i);
                }
            }
            if !lru_cleared.is_empty() {
                for &i in &lru_cleared {
                    b.recorded.remove(&i);
                }
            }
            let check: Vec<(u32, (u32, u32))> = if viewed {
                b.slots
                    .iter()
                    .filter(|(i, _)| !dirty.contains(i))
                    .map(|(&i, &v)| (i, v))
                    .collect()
            } else {
                Vec::new()
            };
            let todo: Vec<(u32, Option<(u32, u32)>)> = dirty
                .into_iter()
                .map(|i| (i, b.slots.get(&i).copied()))
                .collect();
            (todo, check, lru_cleared)
        };
        let mut made_any = !lru_cleared.is_empty();
        for idx in lru_cleared {
            self.clear_stale_slot(idx);
        }
        todo.extend(stale_views(&check).into_iter().map(|(i, v)| (i, Some(v))));
        let (started, left) = (std::time::Instant::now(), self.budget_left(0));
        let mut todo = todo.into_iter();
        for (index, entry) in todo.by_ref() {
            if started.elapsed().as_micros() as u64 > left {
                // Over budget: the rest waits for the next submission.
                self.bindless
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .dirty
                    .insert(index);
                break;
            }
            made_any = true;
            let made = entry.map(|e| (e, self.use_slot(index, e)));
            let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
            match made {
                None => {
                    b.uses.remove(&index);
                }
                Some((_, Some(u))) => {
                    b.uses.insert(index, u);
                }
                Some((e, None)) => self.drop_bindless(&mut b, index, Some(e)),
            }
        }
        self.budget_spend(0, started.elapsed().as_micros() as u64);
        let mut b = self.bindless.lock().unwrap_or_else(|e| e.into_inner());
        b.dirty.extend(todo.map(|(i, _)| i));
        if !made_any && let Some(plan) = &b.plan {
            return Some(plan.clone());
        }
        let (mut reads, mut writes, mut bounces, mut retained) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for u in b.uses.values() {
            reads.extend(u.read.clone());
            writes.extend(u.write.clone());
            bounces.extend(u.bounce.clone());
            retained.extend(u.retained.clone());
        }
        bounces.sort_unstable_by_key(|&(_, addr, _, _)| addr);
        let plan = Arc::new(BindlessPlan {
            reads: merged(reads, READ_GAP),
            writes: merged(writes, 0),
            bounces,
            _retained: retained,
        });
        b.plan = Some(plan.clone());
        Some(plan)
    }

    /// Materializes bindless slot `index` from its view `(vi, generation)`
    /// and finds what it reads and writes; `None` if the slot is no longer
    /// one for bindless access (its view went stale, or is not of a buffer).
    fn use_slot(&self, index: u32, (vi, generation): (u32, u32)) -> Option<SlotUse> {
        use windows::core::Interface;
        let Some(view) = view_at(vi, generation) else {
            self.clear_stale_slot(index);
            return None;
        };
        if !view.is_buffer() {
            return None;
        }
        let unmapped = |view: &View| {
            self.set_unmapped_null(index, view);
            Some(SlotUse {
                unmapped: true,
                ..SlotUse::default()
            })
        };
        let key = u64::from(generation) << 32 | u64::from(vi);
        let mut slot = SlotUse::default();
        match &view {
            &View::Constant(address, size) => {
                let capped = size.min(65_536);
                if !valid_title_address(address, u64::from(capped)) {
                    return unmapped(&view);
                }
                let (mut reads, mut bounces) = (Vec::new(), Vec::new());
                match self.materialize_bindless_cbv(
                    index,
                    key,
                    address,
                    capped,
                    &mut reads,
                    &mut bounces,
                ) {
                    Some(res) => slot.retained = Some(res),
                    None => return unmapped(&view),
                }
                slot.read = reads.pop();
                slot.bounce = bounces.pop();
            }
            View::Placed(false, resource, desc) if resource[0] != 1 => {
                slot.placed = Some(0);
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                if !valid_title_address(address, 1) {
                    self.set_unmapped_null(index, &view);
                    return Some(slot);
                }
                if let Some((shader_view, held)) = placed_texture(&view)
                    && let Some(host_tex) = crate::resource::host_resource(held.as_this())
                {
                    self.ensure_placed_slot(index, key, &shader_view, host_tex.as_raw());
                    slot.placed = Some(held.as_this() as usize);
                    slot.retained = Some(host_tex.clone());
                } else {
                    self.set_unmapped_null(index, &view);
                }
            }
            _ => {
                let Some((address, len)) = buffer_range(&view) else {
                    self.set_unmapped_null(index, &view);
                    return Some(slot);
                };
                if !valid_title_address(address, len) {
                    return unmapped(&view);
                }
                slot.retained = self.ensure_buffer_slot(index, key, &view);
                if let Some(shadow) = crate::shadow::find(address, len) {
                    let clamped = len.min(shadow.left(address)) as usize;
                    if clamped > 0 {
                        slot.read = Some((shadow.clone(), address as usize, clamped));
                        if matches!(view, View::Unordered(..) | View::Placed(true, ..)) {
                            if clamped <= WRITE_CAP {
                                slot.write = Some((shadow, address as usize, clamped));
                            } else {
                                static ONCE: std::sync::Once = std::sync::Once::new();
                                ONCE.call_once(|| {
                                    tracing::warn!(
                                        bytes = clamped,
                                        "a bindless unordered access view too big to copy back"
                                    );
                                });
                            }
                        }
                    }
                }
            }
        }
        Some(slot)
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = *self
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)?;
        if key == 0 {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = crate::shadow::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }
}

/// Whether `[address, address + len)` may be title memory: inside the
/// title's address range, not null.
fn plausible(address: u64, len: u64) -> bool {
    address >= 0x10000
        && len > 0
        && address
            .checked_add(len)
            .is_some_and(|end| end <= (1u64 << 48))
}

/// Whether `[address, address + len)` is title memory: plausible, and in a
/// title buffer or committed. The kernel is asked only outside buffers:
/// in the title's big pools each asking costs milliseconds.
fn valid_title_address(address: u64, len: u64) -> bool {
    plausible(address, len)
        && (crate::shadow::in_buffer(address, len)
            || crate::texture::committed(address as *const u8, 1))
}

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A view description from its words, with the format as the host has it.
fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (base, words): (u64, &[u32; 10]) = match view {
        View::Shader(r, Some(desc)) => (
            crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64,
            desc,
        ),
        View::Placed(false, resource, desc) if resource[0] == 1 => (
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            desc[..10].try_into().ok()?,
        ),
        View::Unordered(r, _, Some(desc)) => {
            let base = crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64;
            return unordered_range(base, desc);
        }
        View::Placed(true, resource, desc) if resource[0] == 1 => {
            let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            return unordered_range(base, desc[..10].try_into().ok()?);
        }
        _ => return None,
    };
    let desc = srv_desc(words);
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let first_off = buffer.FirstElement.checked_mul(element)?;
    let byte_len = u64::from(buffer.NumElements).checked_mul(element)?;
    Some((base.checked_add(first_off)?, byte_len))
}

/// [`buffer_range`] of an unordered access view of memory at `base`.
fn unordered_range(base: u64, words: &[u32; 10]) -> Option<(u64, u64)> {
    let desc = uav_desc(words);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32)?.0
    };
    let first_off = buffer.FirstElement.checked_mul(element)?;
    let byte_len = u64::from(buffer.NumElements).checked_mul(element)?;
    Some((base.checked_add(first_off)?, byte_len))
}

/// The host resource a slot made by [`make`] views, if any.
type Kept = Option<ID3D12Resource>;

fn buffer_srv(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<Kept> {
    let shadow = crate::shadow::find(address, len)?;
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let offset = shadow.offset(address);
    if element == 0 || offset % element != 0 {
        once!(
            offset,
            element,
            "buffer view not on an element of its host copy"
        );
        return None;
    }
    buffer.FirstElement += offset / element;
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
    };
    Some(Some(shadow.buffer.clone()))
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; false if the copy cannot be
/// written by shaders (upload memory).
fn buffer_uav(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<Kept> {
    let shadow = crate::shadow::find(address, len)?;
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return None;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    let offset = shadow.offset(address);
    if element == 0 || offset % element != 0 {
        once!(
            offset,
            element,
            "unordered buffer view not on an element of its host copy"
        );
        return None;
    }
    buffer.FirstElement += offset / element;
    buffer.CounterOffsetInBytes = 0;
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device.CreateUnorderedAccessView(
            &shadow.buffer,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
    Some(Some(shadow.buffer.clone()))
}

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in another format than a
/// typed resource's. A view of a typed texture in another format of its
/// family takes the texture's format.
/// shortcut: such a view (sRGB of UNORM, say) reads without its
/// conversion; cast with castable formats if colors come out wrong.
fn fits(texture: &ID3D12Resource, mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    if family(view) != family(own) {
        once!(view, own, "view format of another family");
        return false;
    }
    if let Some(format) = format
        && !typeless
    {
        *format = d.Format;
    }
    true
}

/// Makes the host descriptor for `view` at `at`: the host resource it
/// views, or `None` if it cannot.
fn make(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    let host = host()?;
    let device = &host.device;
    match view {
        &View::Constant(address, size) => {
            if address == 0 || size == 0 {
                return None;
            }
            let capped = size.min(65_536);
            let shadow = crate::shadow::find(address, u64::from(capped))?;
            let gpu = shadow.gpu(address);
            if gpu % 256 != 0 {
                once!(
                    address = format_args!("{address:#x}"),
                    "constant buffer not 256-byte aligned on the host"
                );
                return None;
            }
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: gpu,
                SizeInBytes: capped.next_multiple_of(256),
            };
            // SAFETY: a slot of our heap; the range lies in the host copy.
            unsafe { device.CreateConstantBufferView(Some(&desc), at) };
            Some(Some(shadow.buffer.clone()))
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::object::This;
            if let Some(texture) = crate::resource::host_resource(resource) {
                let mut desc = desc.as_ref().map(srv_desc);
                if !fits(texture, desc.as_mut().map(|d| &mut d.Format), false) {
                    return None;
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                Some(Some(texture.clone()))
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                buffer_srv(data as u64, len, srv_desc(desc), at)
            } else {
                None
            }
        }
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::object::This;
            let Some(texture) = crate::resource::host_resource(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        buffer_uav(data as u64, len, uav_desc(desc), at)
                    }
                    _ => None,
                };
            };
            let mut desc = desc.as_ref().map(uav_desc);
            if !fits(texture, desc.as_mut().map(|d| &mut d.Format), true) {
                return None;
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            Some(Some(texture.clone()))
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words);
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::Placed(unordered, resource, desc) => {
            if let Some((view, _held)) = placed_texture(view) {
                return make(&view, at);
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                // shortcut: unordered access to a texture the title placed
                // itself is not mirrored; give it a resource as shader
                // views of such textures get when a dispatch writes one.
                once!(
                    dimension = resource[0],
                    "placed unordered texture view not mirrored"
                );
                return None;
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            if *unordered {
                buffer_uav(address, len, uav_desc(words), at)
            } else {
                buffer_srv(address, len, srv_desc(words), at)
            }
        }
    }
}

/// A shader view of a texture the title placed in memory itself, as a view
/// of the resource made for that memory (on first use), together with a held
/// reference so concurrent eviction cannot free the resource while in use.
pub(crate) fn placed_texture(view: &View) -> Option<(View, crate::resource::HeldTexture)> {
    const BUFFER: u32 = 1;
    let View::Placed(false, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let texture = crate::resource::texture_at(address, resource)?;
    let shader_view = View::Shader(texture.as_this() as usize, desc[..10].try_into().ok());
    Some((shader_view, texture))
}

/// A null view of the kind `view` is, for a slot whose view the host
/// cannot make: unordered access (to a buffer or a 2D texture) or a shader
/// view. Writes to it are dropped, reads give zeros.
fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let is_buffer = |r: &usize| crate::resource::buffer_bytes(*r as crate::object::This).is_some();
    // Unordered views: whether a buffer, and the title's view dimension.
    let (buffer, dimension) = match view {
        View::Unordered(r, _, desc) => (
            is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
            desc.map(|d| d[1]),
        ),
        View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
        // Shader views of buffers read nothing through a null buffer view:
        // a null 2D view where the shader expects a buffer is a mismatch
        // the hardware does not survive in general (GPU-based validation
        // reports it).
        View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
            return null_buffer_view(at);
        }
        View::Placed(false, resource, _) if resource[0] == 1 => return null_buffer_view(at),
        // Textures: a null view of the title's dimension (1D, arrays, 3D,
        // cubes), not always 2D.
        View::Shader(_, Some(desc)) => return null_like(desc, at),
        View::Placed(false, _, desc) => {
            return null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
        }
        View::Constant(..) => {
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC::default();
            // SAFETY: a slot of our heap; a zero location makes a null view.
            return unsafe { host.device.CreateConstantBufferView(Some(&desc), at) };
        }
        _ => return null_view(at),
    };
    let desc = if buffer {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        }
    } else {
        null_uav_desc(dimension)
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

/// A null texture UAV of the title's view dimension (1D, 1D or 2D arrays,
/// 3D), 2D if it names none: a 2D null view where the shader writes a 3D
/// texture is a mismatch GPU-based validation reports (the world's wind
/// field, CS_CopyWindFieldBeforeUpdateWithSmooth, 2026-11-24).
fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
        Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        _ => {}
    }
    desc
}

/// A buffer view that reads zeros (raw, as translated shaders mostly read
/// buffers).
fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 1,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A texture view that reads zeros with the dimension (and mip and array
/// counts) of the title's description `words`; 2D if it names none.
fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
    if !TEXTURES.contains(&words[1]) {
        return null_view(at);
    }
    let Some(host) = host() else { return };
    // One mip, one slice or cube, plane 0: fields a null view still checks.
    let mut w = [0u32; 10];
    w[1] = words[1];
    match words[1] {
        6 => {}
        7 => w[5] = 1,
        3 | 5 | 10 => (w[5], w[7]) = (1, 1),
        _ => w[5] = 1,
    }
    let mut desc = srv_desc(&w);
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A view that reads zeros, for slots whose view the host cannot make.
/// shortcut: slots with no view at all (kind unknown) get this null 2D
/// view; null buffer views there did not stop the world's low-address
/// page faults (2026-11-24, run w10), so they are not the cause.
fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "diagnostics")]
    mod texture_record_tests;
    use super::*;
    use crate::descriptor::{
        HeapDesc, copy_simple, create_cbv, create_heap, create_placed_srv, create_srv, create_uav,
        heap,
    };
    use crate::object::{Iid, S_OK, This, guid, slot};
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::core::s;

    const HEAP_IID: Iid = guid(
        0x8efb471d,
        0x616c,
        0x4f49,
        [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
    );
    const RESOURCE_IID: Iid = guid(
        0x696442be,
        0xa72e,
        0x4059,
        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
    );

    #[repr(C)]
    struct ExtendedHeapDesc {
        desc: HeapDesc,
        addr: u64,
        size: u64,
    }

    fn make_shader_heap(count: u32) -> (This, u64) {
        let ext = ExtendedHeapDesc {
            desc: HeapDesc {
                kind: 0,
                count,
                flags: 1,
                nodes: 0,
            },
            addr: 0,
            size: 0,
        };
        let mut out = std::ptr::null();
        assert_eq!(
            create_heap(std::ptr::null(), &ext.desc, &HEAP_IID, &mut out),
            S_OK
        );
        let start = slot::<extern "system" fn(This) -> u64>(out, 0x48)(out);
        (out, start)
    }

    fn make_title_buffer(address: u64, size: u64, uav: bool) -> This {
        let words: [u32; 14] = [
            1, // BUFFER
            0,
            0,
            0,
            size as u32,
            (size >> 32) as u32,
            1,
            0x0001_0001,
            0,
            1,
            0,
            1, // ROW_MAJOR
            if uav { 4 } else { 0 },
            0,
        ];
        let mut out = std::ptr::null();
        assert_eq!(
            crate::resource::create_at_address(
                std::ptr::null(),
                address,
                words.as_ptr().cast(),
                0,
                std::ptr::null(),
                &RESOURCE_IID,
                &mut out,
            ),
            S_OK
        );
        out
    }

    fn compile_probe_cs(device: &ID3D12Device) -> (ID3D12RootSignature, ID3D12PipelineState) {
        const PROBE_HLSL: &str = r#"
#define RS "DescriptorTable(SRV(t0, numDescriptors=1)), \
            DescriptorTable(CBV(b0, numDescriptors=1)), \
            DescriptorTable(SRV(t1, numDescriptors=1)), \
            DescriptorTable(UAV(u0, numDescriptors=1)), \
            DescriptorTable(SRV(t2, numDescriptors=1)), \
            DescriptorTable(CBV(b1, numDescriptors=1))"
ByteAddressBuffer srv_buf : register(t0);
cbuffer Cbv16 : register(b0) { uint4 cb_vals; };
Texture2D<float4> placed_tex : register(t1);
RWByteAddressBuffer uav_buf : register(u0);
ByteAddressBuffer copied_srv : register(t2);
cbuffer NullCbv : register(b1) { uint4 null_vals; };

[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    uint v_srv = srv_buf.Load(0);
    uint v_cbv = cb_vals.x + cb_vals.y + cb_vals.z + cb_vals.w;
    float4 px = placed_tex.Load(int3(0, 0, 0));
    uint v_tex = (uint(round(px.x * 255.0)) << 0)
               | (uint(round(px.y * 255.0)) << 8)
               | (uint(round(px.z * 255.0)) << 16)
               | (uint(round(px.w * 255.0)) << 24);
    uint v_copied = copied_srv.Load(0);
    uint v_null = null_vals.x;
    uav_buf.Store(0, v_srv);
    uav_buf.Store(4, v_cbv);
    uav_buf.Store(8, v_tex);
    uav_buf.Store(12, v_copied);
    uav_buf.Store(16, v_null);
}
"#;
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: static source and entry point strings; out pointer ours.
        unsafe {
            D3DCompile(
                PROBE_HLSL.as_ptr().cast(),
                PROBE_HLSL.len(),
                s!("bindless_probe"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )
            .unwrap();
            let code = code.unwrap();
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).unwrap();
            let pso = device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: bytes.as_ptr().cast(),
                        BytecodeLength: bytes.len(),
                    },
                    ..Default::default()
                })
                .unwrap();
            (root, pso)
        }
    }

    /// A null texture UAV keeps the title's dimension (3D for the world's
    /// wind field), and is 2D when the title names none.
    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        // Multisampled UAVs need a cap; they stay 2D.
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    /// A null 3D UAV made on the real device leaves it alive.
    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4, 0) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4, 0) else {
            return; // no host GPU
        };
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    /// End-to-end GPU execution test proving that buffer SRVs, 16-byte-aligned
    /// CBVs, copied buffer SRVs, `View::Placed` streamed textures, and buffer
    /// UAVs in a shader-visible heap materialize non-null host descriptors,
    /// refresh modified title memory across submissions, copy bindless UAV
    /// writes back to title memory, and map null/invalid addresses safely to 0.
    #[test]
    fn bindless_slots_materialize_refresh_and_copy_back_on_gpu() {
        let Some(host) = host() else {
            return;
        };
        let (title_heap, base) = make_shader_heap(16);
        let slot_handle = |i: u64| base + i * 32;

        // Backing title memory (64 KiB aligned).
        let mem = crate::resource::Memory::zeroed(256 << 10, 64 << 10).unwrap();
        let mem_base = mem.address();
        let srv_addr = mem_base;
        let cbv_16b_addr = mem_base + 0x30; // 16B-aligned, NOT 256B-aligned!
        let copy_srv_addr = mem_base + 0x1000;
        let uav_addr = mem_base + 0x2000;
        let placed_tex_addr = mem_base + (64 << 10);

        let srv_res = make_title_buffer(srv_addr, 256, false);
        let copy_srv_res = make_title_buffer(copy_srv_addr, 256, false);
        let uav_res = make_title_buffer(uav_addr, 256, true);

        // Initial CPU data in title memory.
        // SAFETY: offsets lie within the 256 KiB allocation `mem`.
        unsafe {
            (srv_addr as *mut u32).write(13);
            (cbv_16b_addr as *mut [u32; 4]).write([10, 20, 30, 40]);
            (copy_srv_addr as *mut u32).write(0xCAFE_1234);
            (uav_addr as *mut [u32; 8]).write([0; 8]);
            // First pixel of linear 4x4 RGBA8 placed texture at `placed_tex_addr`.
            (placed_tex_addr as *mut [u8; 4]).write([0x12, 0x34, 0x56, 0x78]);
        }

        // Raw ByteAddressBuffer SRV/UAV descriptions in host D3D12 layout.
        let srv_desc_host = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 0,
                    NumElements: 16,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        let uav_desc_host = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: 16,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        // SAFETY: both D3D12 view descriptions are 40-byte POD values (`[u32; 10]`).
        let (raw_srv_words, raw_uav_words): ([u32; 10], [u32; 10]) = unsafe {
            (
                std::ptr::from_ref(&srv_desc_host)
                    .cast::<[u32; 10]>()
                    .read(),
                std::ptr::from_ref(&uav_desc_host)
                    .cast::<[u32; 10]>()
                    .read(),
            )
        };

        // Slot 0: Buffer SRV.
        create_srv(std::ptr::null(), srv_res, &raw_srv_words, slot_handle(0));
        // Slot 1: 16B-aligned CBV (size 64 bytes, address % 256 == 0x30).
        create_cbv(
            std::ptr::null(),
            &[cbv_16b_addr as u32, (cbv_16b_addr >> 32) as u32, 64, 0],
            slot_handle(1),
        );
        // Slot 2: Placed 4x4 R8G8B8A8_UNORM linear texture (`layout = 0x100`).
        let placed_res_words: [u32; 14] = [
            3, // TEXTURE2D
            0,
            0,
            0,
            4,
            0,
            4,
            0x0001_0001,
            DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
            1,
            0,
            0x100, // console LINEAR layout
            0,
            0,
        ];
        let tex_srv_host = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    PlaneSlice: 0,
                    ResourceMinLODClamp: 0.0,
                },
            },
        };
        let mut placed_view_words = [0u32; 16];
        // SAFETY: `D3D12_SHADER_RESOURCE_VIEW_DESC` is 40 bytes (`[u32; 10]`).
        placed_view_words[..10].copy_from_slice(&unsafe {
            std::ptr::from_ref(&tex_srv_host).cast::<[u32; 10]>().read()
        });
        placed_view_words[10] = placed_tex_addr as u32;
        placed_view_words[11] = (placed_tex_addr >> 32) as u32;
        create_placed_srv(
            std::ptr::null(),
            &placed_res_words,
            &placed_view_words,
            slot_handle(2),
        );
        // Slot 3: Buffer UAV.
        create_uav(
            std::ptr::null(),
            uav_res,
            std::ptr::null(),
            &raw_uav_words,
            slot_handle(3),
        );
        // Slot 6 -> copied to Slot 4 via `copy_simple`.
        create_srv(
            std::ptr::null(),
            copy_srv_res,
            &raw_srv_words,
            slot_handle(6),
        );
        copy_simple(std::ptr::null(), 1, slot_handle(4), slot_handle(6), 0);
        // Slot 5: Unmapped/null CBV (address = 0) -> must read 0 safely.
        create_cbv(std::ptr::null(), &[0, 0, 256, 0], slot_handle(5));

        let host_heap = heap(title_heap).host_arc().unwrap();
        // Before bindless preparation, `mirror(..., false)` deferred all
        // `View::is_buffer()` slots (`made[idx] == 0`).
        for idx in 0..=5 {
            assert_eq!(
                host_heap.made.lock().unwrap()[idx],
                0,
                "slot {idx} starts deferred"
            );
        }

        let (root, pso) = compile_probe_cs(&host.device);
        let list = host.new_list().unwrap();

        let record_probe = |list: &crate::host::HostList| {
            host_heap.prepare_bindless_record(list);
            list.defer_bindless(&host_heap, true);
            // SAFETY: open command list, shader-visible heap, and matching root/PSO.
            unsafe {
                let gpu_start = host_heap.heap.GetGPUDescriptorHandleForHeapStart().ptr;
                let gpu_at = |slot_idx: u32| D3D12_GPU_DESCRIPTOR_HANDLE {
                    ptr: gpu_start + u64::from(slot_idx) * u64::from(host_heap.step),
                };
                list.list
                    .SetDescriptorHeaps(&[Some(host_heap.heap.clone())]);
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list.SetComputeRootDescriptorTable(0, gpu_at(0));
                list.list.SetComputeRootDescriptorTable(1, gpu_at(1));
                list.list.SetComputeRootDescriptorTable(2, gpu_at(2));
                list.list.SetComputeRootDescriptorTable(3, gpu_at(3));
                list.list.SetComputeRootDescriptorTable(4, gpu_at(4));
                list.list.SetComputeRootDescriptorTable(5, gpu_at(5));
                list.list.Dispatch(1, 1, 1);
            }
            list.close();
        };

        record_probe(&list);
        host.execute(&[&list]);
        list.reset();

        // SAFETY: GPU finished and `Host::execute` copied UAV writes back to `uav_addr`.
        let out1 = unsafe { (uav_addr as *const [u32; 5]).read_volatile() };
        assert_eq!(
            out1[0], 13,
            "bindless buffer SRV materialized and refreshed"
        );
        assert_eq!(
            out1[1], 100,
            "16B-aligned bindless CBV materialized via bounce"
        );
        assert_eq!(
            out1[2], 0x7856_3412,
            "placed streamed texture uploaded and sampled"
        );
        assert_eq!(out1[3], 0xCAFE_1234, "copied bindless SRV materialized");
        assert_eq!(out1[4], 0, "null-address CBV safely reads zero");

        // Modify CPU memory for both the SRV buffer and the 16B-aligned CBV,
        // overwrite slot 5 with a `u64::MAX` garbage address CBV, and release
        // `copy_srv_res` so slot 4 goes stale -> both slots 4 and 5 read 0!
        create_cbv(
            std::ptr::null(),
            &[u32::MAX, u32::MAX, 256, 0],
            slot_handle(5),
        );
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        assert_eq!(release(copy_srv_res), 0);
        // SAFETY: offsets lie within `mem`.
        unsafe {
            (srv_addr as *mut u32).write_volatile(500);
            (cbv_16b_addr as *mut [u32; 4]).write_volatile([100, 200, 300, 400]);
        }

        // Released views are looked for once a frame.
        crate::queue::pass_frame();
        record_probe(&list);
        host.execute(&[&list]);
        list.reset();

        // SAFETY: GPU finished and copied back.
        let out2 = unsafe { (uav_addr as *const [u32; 5]).read_volatile() };
        assert_eq!(
            out2[0], 500,
            "dirty SRV title memory refreshed on second submit"
        );
        assert_eq!(
            out2[1], 1000,
            "dirty 16B-aligned CBV refreshed on second submit"
        );
        assert_eq!(out2[3], 0, "released resource slot pruned and reads zero");
        assert_eq!(
            out2[4], 0,
            "overflowing u64::MAX garbage address safely reads zero"
        );

        assert_eq!(release(srv_res), 0);
        assert_eq!(release(uav_res), 0);
        assert_eq!(release(title_heap), 0);
    }

    /// Submissions with nothing changed reuse a heap's bindless plan (no
    /// slot is made or range found again); writing a slot makes a new one,
    /// whose reads of one host copy are merged into one range.
    #[test]
    fn bindless_plan_is_remade_only_when_slots_change() {
        if host().is_none() {
            return;
        }
        let (title_heap, base) = make_shader_heap(8);
        let mem = crate::resource::Memory::zeroed(256 << 10, 64 << 10).unwrap();
        let cbv = |at: u64, slot: u64| {
            create_cbv(
                std::ptr::null(),
                &[at as u32, (at >> 32) as u32, 256, 0],
                base + slot * 32,
            );
        };
        cbv(mem.address(), 0);
        let host_heap = heap(title_heap).host_arc().unwrap();
        let first = host_heap.bindless_plan().unwrap();
        assert_eq!(first.reads.len(), 1);
        assert_eq!(first.reads[0].2, 256);
        assert!(first.writes.is_empty(), "constant buffers are not written");
        // Other tests may release resources meanwhile, which remakes plans.
        let reused = (0..100).any(|_| {
            let a = host_heap.bindless_plan().unwrap();
            Arc::ptr_eq(&a, &host_heap.bindless_plan().unwrap())
        });
        assert!(reused, "unchanged slots reuse the plan");

        // Slot 0 marked unmade: were it made again, it would be made anew.
        host_heap.made.lock().unwrap()[0] = 0;
        cbv(mem.address() + 0x1000, 1);
        let second = host_heap.bindless_plan().unwrap();
        assert!(
            !Arc::ptr_eq(&first, &second),
            "a written slot remakes the plan"
        );
        assert_eq!(second.reads.len(), 1, "nearby reads merge");
        assert_eq!(second.reads[0].1, mem.address() as usize);
        assert_eq!(second.reads[0].2, 0x1100);
        assert_eq!(
            host_heap.made.lock().unwrap()[0],
            0,
            "only the written slot is made again"
        );
        assert_ne!(host_heap.made.lock().unwrap()[1], 0);

        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        assert_eq!(release(title_heap), 0);
    }

    /// Placed textures reached through slots upload the first time, then
    /// only after [`RECHECK_FRAMES`] (a few per frame), or when another
    /// texture takes the object's address.
    #[test]
    fn bindless_texture_uploads_are_throttled() {
        let Some(host) = host() else {
            return;
        };
        let Some(heap) = HostHeap::new(false, 4, 0) else {
            return;
        };
        let (a, _) = host.shared_buffer(1 << 16).unwrap();
        let (b, _) = host.shared_buffer(1 << 16).unwrap();
        let tex = 0x1000 as crate::object::This;
        let target = |t: crate::object::This| RecheckTarget::Placed(t as usize);
        assert!(
            heap.upload_due(tex, &a, 10, target(tex)),
            "first reach uploads"
        );
        assert!(!heap.upload_due(tex, &a, 10, target(tex)));
        assert!(!heap.upload_due(tex, &a, 10 + RECHECK_FRAMES - 1, target(tex)));
        assert!(
            heap.upload_due(tex, &b, 11, target(tex)),
            "a new texture at the address"
        );
        assert!(
            heap.upload_due(tex, &b, 11 + RECHECK_FRAMES, target(tex)),
            "looked at again"
        );
        // At most `RECHECKS` known textures per frame.
        let frame = 1000;
        let many: Vec<_> = (0..2 * RECHECKS as usize)
            .map(|i| {
                let t = (0x2000 + i) as crate::object::This;
                assert!(heap.upload_due(t, &a, 0, target(t)));
                t
            })
            .collect();
        let again = many
            .iter()
            .filter(|&&t| heap.upload_due(t, &a, frame, target(t)))
            .count();
        assert_eq!(again, RECHECKS as usize);
    }

    /// A list queues each heap once, keeping whether any draw may write.
    #[test]
    fn bindless_heaps_queue_once_per_list() {
        let Some(host) = host() else {
            return;
        };
        let (title_heap, _) = make_shader_heap(8);
        let host_heap = heap(title_heap).host_arc().unwrap();
        let list = host.new_list().unwrap();
        list.defer_bindless(&host_heap, false);
        list.defer_bindless(&host_heap, true);
        list.defer_bindless(&host_heap, false);
        assert_eq!(list.bindless_queued(), vec![true]);
        list.close();
        host.execute(&[&list]);
        list.reset();
        assert!(list.bindless_queued().is_empty());

        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        assert_eq!(release(title_heap), 0);
    }

    /// Verifies `PLACED_TEXTURES` entries overlapping a remapped virtual range
    /// are evicted on `title_memory_remapped` while non-overlapping entries stay.
    #[test]
    fn placed_textures_evict_on_title_memory_remapped() {
        if host().is_none() {
            return;
        }
        let mem = crate::resource::Memory::zeroed(256 << 10, 64 << 10).unwrap();
        let addr_a = mem.address();
        let addr_b = mem.address() + (128 << 10);
        let desc: [u32; 14] = [
            3, // TEXTURE2D
            0,
            0,
            0,
            16,
            0,
            16,
            0x0001_0001,
            DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
            1,
            0,
            0x100, // LINEAR
            0,
            0,
        ];

        assert!(crate::resource::texture_at(addr_a, &desc).is_some());
        assert!(crate::resource::texture_at(addr_b, &desc).is_some());
        assert!(crate::resource::has_placed_texture(addr_a, &desc));
        assert!(crate::resource::has_placed_texture(addr_b, &desc));

        let epoch_before = crate::resource::placed_epoch();
        // Remap 256 bytes inside `addr_a`'s footprint (not even starting at `addr_a`!).
        crate::watch::title_memory_remapped((addr_a + 128) as usize, 256);

        assert!(
            !crate::resource::has_placed_texture(addr_a, &desc),
            "overlapping placed texture must be evicted"
        );
        assert!(
            crate::resource::has_placed_texture(addr_b, &desc),
            "non-overlapping placed texture must remain cached"
        );
        assert!(crate::resource::placed_epoch() > epoch_before);

        // Clean up `addr_b` too.
        crate::watch::title_memory_remapped(addr_b as usize, 64 << 10);
        assert!(!crate::resource::has_placed_texture(addr_b, &desc));
    }

    /// A `HeldTexture` returned by `texture_at` keeps the placed resource
    /// alive across concurrent eviction by `title_memory_remapped`.
    #[test]
    fn placed_texture_handle_survives_eviction() {
        if host().is_none() {
            return;
        }
        let mem = crate::resource::Memory::zeroed(128 << 10, 64 << 10).unwrap();
        let addr = mem.address();
        let desc: [u32; 14] = [
            3, // TEXTURE2D
            0,
            0,
            0,
            16,
            0,
            16,
            0x0001_0001,
            DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
            1,
            0,
            0x100, // LINEAR
            0,
            0,
        ];

        let held = crate::resource::texture_at(addr, &desc).unwrap();
        assert!(crate::resource::has_placed_texture(addr, &desc));

        // Evict while the caller still holds `held`.
        crate::watch::title_memory_remapped(addr as usize, 64 << 10);
        assert!(!crate::resource::has_placed_texture(addr, &desc));

        // The caller's handle still points to a live resource and host texture.
        let host_tex = crate::resource::host_resource(held.as_this()).unwrap();
        // SAFETY: `held` keeps the resource (and its host texture) alive.
        let host_desc = unsafe { host_tex.GetDesc() };
        assert_eq!((host_desc.Width, host_desc.Height), (16, 16));
        drop(held);
    }

    /// Slots filled, overwritten, or cleared by raw byte copies into the
    /// title heap (bypassing `mirror()`) are detected by `bindless_plan`.
    #[test]
    fn bindless_slots_detect_byte_copies_and_overwrites() {
        if host().is_none() {
            return;
        }
        let (title_heap, base) = make_shader_heap(8);
        let mem = crate::resource::Memory::zeroed(256 << 10, 64 << 10).unwrap();
        let addr_a = mem.address();
        let addr_b = mem.address() + (128 << 10);
        let mut staging = [0u64; 8]; // two 32-byte staging slots outside the shader heap
        let stage_a = staging.as_mut_ptr() as u64;
        let stage_b = stage_a + 32;
        create_cbv(
            std::ptr::null(),
            &[addr_a as u32, (addr_a >> 32) as u32, 256, 0],
            stage_a,
        );
        create_cbv(
            std::ptr::null(),
            &[addr_b as u32, (addr_b >> 32) as u32, 256, 0],
            stage_b,
        );

        let host_heap = heap(title_heap).host_arc().unwrap();
        let slot1 = base + 32;

        // Raw byte copy into an empty shader-visible slot (no API mirror call).
        // SAFETY: both `stage_a` and `slot1` are valid 32-byte descriptor slots.
        unsafe { std::ptr::copy_nonoverlapping(stage_a as *const u8, slot1 as *mut u8, 32) };
        let p1 = host_heap.bindless_plan().unwrap();
        assert_eq!(p1.reads.len(), 1);
        assert_eq!(p1.reads[0].1, addr_a as usize);
        assert_ne!(host_heap.made.lock().unwrap()[1], 0);

        // Raw byte overwrite of an already-tracked slot with another view.
        // SAFETY: both `stage_b` and `slot1` are valid 32-byte descriptor slots.
        unsafe { std::ptr::copy_nonoverlapping(stage_b as *const u8, slot1 as *mut u8, 32) };
        let p2 = host_heap.bindless_plan().unwrap();
        assert!(!Arc::ptr_eq(&p1, &p2));
        assert_eq!(p2.reads.len(), 1);
        assert_eq!(p2.reads[0].1, addr_b as usize);

        // Raw byte zeroing removes the tracked slot and clears its host descriptor.
        // SAFETY: `slot1` is a valid 32-byte descriptor slot.
        unsafe { (slot1 as *mut u8).write_bytes(0, 32) };
        let p3 = host_heap.bindless_plan().unwrap();
        assert!(p3.reads.is_empty());
        assert_eq!(host_heap.made.lock().unwrap()[1], 0);

        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        assert_eq!(release(title_heap), 0);
    }

    /// Misaligned bindless CBVs on write-watched memory copy only when their
    /// pages change, and cleared slots recycle their bounce slices.
    #[test]
    fn bindless_cbv_bounce_skips_unchanged_watched_pages_and_recycles_slices() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        if host().is_none() {
            return;
        }
        // SAFETY: fresh 64 KiB write-watched allocation, freed before returning.
        let base_ptr = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                crate::watch::PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(base_ptr, 0);
        let cbv_addr = (base_ptr + 0x30) as u64; // misaligned (not 256B-aligned)
        // SAFETY: inside the 64 KiB allocation above.
        unsafe { (cbv_addr as *mut u32).write_volatile(0x1122_3344) };

        let (title_heap, base) = make_shader_heap(8);
        create_cbv(
            std::ptr::null(),
            &[cbv_addr as u32, (cbv_addr >> 32) as u32, 64, 0],
            base,
        );
        let host_heap = heap(title_heap).host_arc().unwrap();
        let plan = host_heap.bindless_plan().unwrap();
        assert_eq!(plan.bounces.len(), 1);
        let (cpu, _, _, ref copied_at) = plan.bounces[0];

        // First copy populates the bounce slice and records the watch asking.
        plan.copy_constants();
        let first_asking = copied_at.load(Ordering::Relaxed);
        assert_ne!(first_asking, 0);
        // SAFETY: bounce slice is mapped CPU-writable memory of >= 64 bytes.
        assert_eq!(unsafe { (cpu as *const u32).read_volatile() }, 0x1122_3344);

        // Second copy with no CPU writes must skip copying (asking unchanged).
        plan.copy_constants();
        assert_eq!(copied_at.load(Ordering::Relaxed), first_asking);

        // Writing the watched page causes the next copy to pick up the new value.
        // SAFETY: inside the 64 KiB allocation above.
        unsafe { (cbv_addr as *mut u32).write_volatile(0x5566_7788) };
        plan.copy_constants();
        assert!(copied_at.load(Ordering::Relaxed) > first_asking);
        // SAFETY: bounce slice is mapped CPU-writable memory of >= 64 bytes.
        assert_eq!(unsafe { (cpu as *const u32).read_volatile() }, 0x5566_7788);

        // Clearing slot 0 recycles its bounce slice so slot 1 reuses it.
        // SAFETY: slot 0 is a valid 32-byte descriptor slot in `title_heap`.
        unsafe { (base as *mut u8).write_bytes(0, 32) };
        let _ = host_heap.bindless_plan().unwrap();
        create_cbv(
            std::ptr::null(),
            &[cbv_addr as u32, (cbv_addr >> 32) as u32, 64, 0],
            base + 32,
        );
        let plan2 = host_heap.bindless_plan().unwrap();
        assert_eq!(plan2.bounces.len(), 1);
        assert_eq!(
            plan2.bounces[0].0, cpu,
            "slot 1 must reuse slot 0's recycled bounce slice"
        );

        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        assert_eq!(release(title_heap), 0);
        // SAFETY: frees our `VirtualAlloc` region.
        unsafe { VirtualFree(base_ptr as *mut _, 0, MEM_RELEASE) };
    }

    /// An ordinary resource-backed CPU texture SRV reached only through a
    /// shader-visible bindless heap schedules its initial upload during
    /// `prepare_bindless_record` and re-uploads after `RECHECK_FRAMES`.
    #[test]
    fn ordinary_bindless_texture_srv_uploads_and_refreshes_on_gpu() {
        const TEX_PROBE_HLSL: &str = r#"
#define RS "DescriptorTable(SRV(t0, numDescriptors=1)), \
            DescriptorTable(UAV(u0, numDescriptors=1))"
Texture2D<float4> tex : register(t0);
RWByteAddressBuffer out_buf : register(u0);

[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    float4 px = tex.Load(int3(0, 0, 0));
    uint packed = (uint(round(px.x * 255.0)) << 0)
                | (uint(round(px.y * 255.0)) << 8)
                | (uint(round(px.z * 255.0)) << 16)
                | (uint(round(px.w * 255.0)) << 24);
    out_buf.Store(0, packed);
}
"#;
        let Some(host) = host() else {
            return;
        };
        let (title_heap, base) = make_shader_heap(8);
        let slot_handle = |i: u64| base + i * 32;

        let mem = crate::resource::Memory::zeroed(128 << 10, 64 << 10).unwrap();
        let uav_addr = mem.address();
        let tex_addr = mem.address() + (64 << 10);
        // SAFETY: offsets lie within `mem`.
        unsafe {
            (uav_addr as *mut u32).write(0);
            (tex_addr as *mut [u8; 4]).write([0x12, 0x34, 0x56, 0x78]);
        }

        let uav_res = make_title_buffer(uav_addr, 256, true);
        let tex_desc_words: [u32; 14] = [
            3, // TEXTURE2D
            0,
            0,
            0,
            4,
            0,
            4,
            0x0001_0001,
            DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
            1,
            0,
            0x100, // console LINEAR layout
            0,
            0,
        ];
        let mut tex_res = std::ptr::null();
        assert_eq!(
            crate::resource::create_at_address(
                std::ptr::null(),
                tex_addr,
                tex_desc_words.as_ptr().cast(),
                0,
                std::ptr::null(),
                &RESOURCE_IID,
                &mut tex_res,
            ),
            S_OK
        );

        let tex_srv_host = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    PlaneSlice: 0,
                    ResourceMinLODClamp: 0.0,
                },
            },
        };
        let uav_desc_host = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: 16,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        // SAFETY: both D3D12 view descriptions are 40-byte POD values (`[u32; 10]`).
        let (tex_srv_words, uav_words): ([u32; 10], [u32; 10]) = unsafe {
            (
                std::ptr::from_ref(&tex_srv_host).cast::<[u32; 10]>().read(),
                std::ptr::from_ref(&uav_desc_host)
                    .cast::<[u32; 10]>()
                    .read(),
            )
        };
        create_srv(std::ptr::null(), tex_res, &tex_srv_words, slot_handle(0));
        create_uav(
            std::ptr::null(),
            uav_res,
            std::ptr::null(),
            &uav_words,
            slot_handle(1),
        );

        let mut code: Option<ID3DBlob> = None;
        // SAFETY: static HLSL source and entry point; out pointer ours.
        let (root, pso) = unsafe {
            D3DCompile(
                TEX_PROBE_HLSL.as_ptr().cast(),
                TEX_PROBE_HLSL.len(),
                s!("ordinary_tex_probe"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )
            .unwrap();
            let code = code.unwrap();
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes).unwrap();
            let pso: ID3D12PipelineState = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: bytes.as_ptr().cast(),
                        BytecodeLength: bytes.len(),
                    },
                    ..Default::default()
                })
                .unwrap();
            (root, pso)
        };

        let host_heap = heap(title_heap).host_arc().unwrap();
        let list = host.new_list().unwrap();
        let run_probe = |list: &crate::host::HostList, expect_scheduled: bool| {
            host_heap.prepare_bindless_record(list);
            if expect_scheduled {
                assert!(
                    host_heap.pending_texture_refreshes().iter().any(|(identity, _)| *identity == tex_res as usize),
                    "ordinary bindless texture SRV must leave a live upload request"
                );
            }
            list.defer_bindless(&host_heap, true);
            // SAFETY: open command list, shader-visible heap, and matching root/PSO.
            unsafe {
                let gpu_start = host_heap.heap.GetGPUDescriptorHandleForHeapStart().ptr;
                let gpu_at = |slot_idx: u32| D3D12_GPU_DESCRIPTOR_HANDLE {
                    ptr: gpu_start + u64::from(slot_idx) * u64::from(host_heap.step),
                };
                list.list
                    .SetDescriptorHeaps(&[Some(host_heap.heap.clone())]);
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list.SetComputeRootDescriptorTable(0, gpu_at(0));
                list.list.SetComputeRootDescriptorTable(1, gpu_at(1));
                list.list.Dispatch(1, 1, 1);
            }
            list.close();
            host.execute(&[list]);
            list.reset();
        };

        // First draw must upload the ordinary CPU-backed texture and sample it.
        run_probe(&list, true);
        // SAFETY: GPU finished and copied UAV output back to `uav_addr`.
        let first = unsafe { (uav_addr as *const u32).read_volatile() };
        assert_eq!(first, 0x7856_3412);

        // Modify CPU bytes and advance past the normal recheck interval.
        // SAFETY: inside `mem`.
        unsafe { (tex_addr as *mut [u8; 4]).write([0x87, 0x65, 0x43, 0x21]) };
        for _ in 0..RECHECK_FRAMES {
            crate::queue::pass_frame();
        }
        run_probe(&list, true);
        // SAFETY: GPU finished and copied UAV output back to `uav_addr`.
        let second = unsafe { (uav_addr as *const u32).read_volatile() };
        assert_eq!(second, 0x2143_6587);

        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        assert_eq!(release(tex_res), 0);
        assert_eq!(release(uav_res), 0);
        assert_eq!(release(title_heap), 0);
    }
}
