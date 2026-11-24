//! Aggregate descriptor demand from the rebuilt-table path.
//!
//! VICTORY_DESCRIPTOR_DEMAND=1 enables this in a diagnostics build. The
//! exact history and trace contain descriptor metadata only. No GPU work
//! or constant-buffer bytes are read, and the history never owns a view.

use std::collections::{HashMap, hash_map::Entry};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use super::{Heap, VIEW_TAG, VIEWS, VIEWS_KIND, View};

const SCAN_LIMIT: u32 = 4_096;
const HISTORY_TABLES: usize = 256;
const HISTORY_SLOTS: usize = 65_536;

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("VICTORY_DESCRIPTOR_DEMAND").is_ok_and(|v| v == "1"))
}

/// Unique within a run, independent of object or title-memory address reuse.
pub(crate) fn identity() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Relaxed)
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Outcome {
    SharedNew,
    SharedReuse,
    LocalNew,
    LocalReuse,
    NoRange,
}

impl Outcome {
    const ALL: [Self; 5] = [
        Self::SharedNew,
        Self::SharedReuse,
        Self::LocalNew,
        Self::LocalReuse,
        Self::NoRange,
    ];

    fn reserved(self) -> bool {
        matches!(self, Self::SharedNew | Self::LocalNew)
    }
}

#[derive(Clone, Copy, Debug)]
enum Slot {
    Empty,
    Stale,
    Unknown,
    Cbv,
    TextureSrv,
    TextureUav,
    BufferSrv,
    BufferUav,
    CounterUav,
    PlacedSrv,
    PlacedUav,
}

impl Slot {
    const ALL: [Self; 11] = [
        Self::Empty,
        Self::Stale,
        Self::Unknown,
        Self::Cbv,
        Self::TextureSrv,
        Self::TextureUav,
        Self::BufferSrv,
        Self::BufferUav,
        Self::CounterUav,
        Self::PlacedSrv,
        Self::PlacedUav,
    ];

    fn of(view: &View) -> Self {
        match view {
            View::Constant(..) => Self::Cbv,
            View::Shader(0, _) | View::Unordered(0, 0, _) => Self::Unknown,
            View::Unordered(_, counter, _) if *counter != 0 => Self::CounterUav,
            View::Shader(..) if view.is_buffer() => Self::BufferSrv,
            View::Unordered(..) if view.is_buffer() => Self::BufferUav,
            View::Shader(..) => Self::TextureSrv,
            View::Unordered(..) => Self::TextureUav,
            View::Placed(false, ..) => Self::PlacedSrv,
            View::Placed(true, ..) => Self::PlacedUav,
            View::Sampler(..) | View::ConsoleSampler(..) => Self::Unknown,
        }
    }

    fn family(self) -> u8 {
        match self {
            Self::Empty => 0,
            Self::Cbv => 1,
            Self::TextureSrv | Self::TextureUav => 2,
            Self::BufferSrv | Self::BufferUav | Self::CounterUav => 4,
            Self::PlacedSrv | Self::PlacedUav => 8,
            Self::Stale | Self::Unknown => 16,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Composition {
    Empty,
    CbvOnly,
    Textures,
    Buffers,
    Placed,
    Mixed,
    Unresolved,
    Uninspected,
}

impl Composition {
    const ALL: [Self; 8] = [
        Self::Empty,
        Self::CbvOnly,
        Self::Textures,
        Self::Buffers,
        Self::Placed,
        Self::Mixed,
        Self::Unresolved,
        Self::Uninspected,
    ];

    fn of(families: u8) -> Self {
        match families {
            0 => Self::Empty,
            1 => Self::CbvOnly,
            2 => Self::Textures,
            4 => Self::Buffers,
            8 => Self::Placed,
            n if n & 16 != 0 => Self::Unresolved,
            _ => Self::Mixed,
        }
    }
}

type Words = [u32; 3];
type Snapshot = Arc<[Words]>;

pub(crate) struct Request {
    heap: u64,
    list: u64,
    table: u64,
    width: u32,
    cbv: bool,
    resolved_textures: bool,
    inspection: Option<Inspection>,
}

struct Inspection {
    records: Snapshot,
    slots: [u32; Slot::ALL.len()],
    composition: Composition,
}

impl Request {
    pub(crate) fn new(
        heap: &Heap,
        heap_id: u64,
        list: u64,
        table: u64,
        width: u32,
        cbv: bool,
    ) -> Self {
        Self {
            heap: heap_id,
            list,
            table,
            width,
            cbv,
            resolved_textures: false,
            inspection: Inspection::read(heap, table, width),
        }
    }

    /// The production resolver accepted this set of view records. Native
    /// materialization or the allocator may still fail.
    pub(crate) fn resolved_textures(&mut self) {
        self.resolved_textures = true;
    }

    pub(crate) fn record(request: Option<Self>, outcome: Outcome) {
        let Some(request) = request else { return };
        let failure = matches!(outcome, Outcome::NoRange);
        HISTORY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Report::default)
            .add(request, outcome);
        static FIRST_FAILURE: AtomicBool = AtomicBool::new(true);
        if failure && FIRST_FAILURE.swap(false, Relaxed) {
            log("first failed rebuilt table", crate::queue::presented());
        }
    }
}

impl Inspection {
    fn read(heap: &Heap, table: u64, width: u32) -> Option<Self> {
        if heap.desc.kind != VIEWS_KIND || width == 0 || width > SCAN_LIMIT {
            return None;
        }
        let offset = table.checked_sub(heap.start)?;
        if !offset.is_multiple_of(32) {
            return None;
        }
        heap.index(table.checked_add(u64::from(width - 1) * 32)?)?;
        let registry = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
        let mut records = Vec::with_capacity(width as usize);
        let mut slots = [0; Slot::ALL.len()];
        let mut families = 0;
        for k in 0..width {
            let at = table + u64::from(k) * 32;
            // SAFETY: the descriptor is inside the checked title heap span.
            let words: Words = unsafe { (at as *const Words).read_unaligned() };
            let [tag, id, generation] = words;
            let slot = if words == [0; 3] {
                Slot::Empty
            } else if tag != VIEW_TAG {
                Slot::Unknown
            } else {
                registry
                    .list
                    .get(id as usize)
                    .filter(|(_, current)| *current == generation)
                    .map_or(Slot::Stale, |(view, _)| Slot::of(view))
            };
            records.push(words);
            slots[slot as usize] += 1;
            families |= slot.family();
        }
        Some(Self {
            records: records.into(),
            slots,
            composition: Composition::of(families),
        })
    }
}

#[derive(Default)]
struct History<K, V> {
    values: HashMap<K, V>,
    slots: usize,
}

impl<K: Eq + Hash, V: Default> History<K, V> {
    fn entry(&mut self, key: K, width: u32) -> Option<&mut V> {
        let full =
            self.values.len() >= HISTORY_TABLES || self.slots + width as usize > HISTORY_SLOTS;
        match self.values.entry(key) {
            Entry::Occupied(entry) => Some(entry.into_mut()),
            Entry::Vacant(_) if full => None,
            Entry::Vacant(entry) => {
                self.slots += width as usize;
                Some(entry.insert(V::default()))
            }
        }
    }
}

#[derive(Default)]
struct Seen {
    first_list: u64,
    multiple_lists: bool,
}

#[derive(Debug, Default)]
struct Counts {
    tables: u64,
    slots: u64,
}

impl Counts {
    fn add(&mut self, width: u32) {
        self.tables += 1;
        self.slots += u64::from(width);
    }
}

#[derive(Default)]
struct Group {
    total: Counts,
    reserved: Counts,
    cross_list: Counts,
    cross_list_reserved: Counts,
    same_list: Counts,
    duplicate_untracked: Counts,
    resolved_textures: Counts,
    generation_changed: u64,
}

const WIDTHS: [u32; 8] = [1, 4, 16, 64, 256, 1_024, SCAN_LIMIT, u32::MAX];

#[derive(Debug, Default)]
struct WidthCounts {
    requested: Counts,
    reserved: Counts,
    no_range: Counts,
}

#[derive(Default)]
struct Report {
    outcome: [Counts; Outcome::ALL.len()],
    widths: [WidthCounts; WIDTHS.len()],
    kinds: [[Group; 2]; Composition::ALL.len()],
    slots: [u64; Slot::ALL.len()],
    same_records: History<(u64, Snapshot), Seen>,
    source: History<(u64, u64, u32), Snapshot>,
    shader_cbv: Counts,
    recorded_cbv: Counts,
    generation_compared: Counts,
    generation_changed: u64,
    generation_untracked: Counts,
}

impl Report {
    fn add(&mut self, request: Request, outcome: Outcome) {
        let Request {
            heap,
            list,
            table,
            width,
            cbv,
            inspection,
            resolved_textures,
        } = request;
        self.outcome[outcome as usize].add(width);
        let band = &mut self.widths[WIDTHS.partition_point(|&upper| upper < width)];
        band.requested.add(width);
        if outcome.reserved() {
            band.reserved.add(width);
        }
        if matches!(outcome, Outcome::NoRange) {
            band.no_range.add(width);
        }
        let composition = inspection
            .as_ref()
            .map_or(Composition::Uninspected, |i| i.composition);
        let recorded_cbv = inspection
            .as_ref()
            .is_some_and(|i| i.slots[Slot::Cbv as usize] > 0);
        if cbv {
            self.shader_cbv.add(width);
        }
        if recorded_cbv {
            self.recorded_cbv.add(width);
        }
        let group = &mut self.kinds[composition as usize][usize::from(cbv || recorded_cbv)];
        group.total.add(width);
        if outcome.reserved() {
            group.reserved.add(width);
        }
        if resolved_textures {
            group.resolved_textures.add(width);
        }
        let Some(Inspection { records, slots, .. }) = inspection else {
            return;
        };
        for (total, n) in self.slots.iter_mut().zip(slots) {
            *total += u64::from(n);
        }
        match self.same_records.entry((heap, records.clone()), width) {
            Some(seen) if seen.first_list == 0 => seen.first_list = list,
            Some(seen) if seen.first_list != list || seen.multiple_lists => {
                seen.multiple_lists = true;
                group.cross_list.add(width);
                if outcome.reserved() {
                    group.cross_list_reserved.add(width);
                }
            }
            Some(_) => group.same_list.add(width),
            None => group.duplicate_untracked.add(width),
        }
        if let Some(previous) = self.source.entry((heap, table, width), width) {
            if !previous.is_empty() {
                self.generation_compared.add(width);
                let changed = previous
                    .iter()
                    .zip(records.iter())
                    .filter(|(a, b)| {
                        a[0] == VIEW_TAG && b[0] == VIEW_TAG && a[1] == b[1] && a[2] != b[2]
                    })
                    .count() as u64;
                self.generation_changed += changed;
                group.generation_changed += changed;
            }
            *previous = records;
        } else {
            self.generation_untracked.add(width);
        }
    }

    fn log(self, reason: &str, frame: u64) {
        let outcome = Outcome::ALL.map(|o| (o, &self.outcome[o as usize]));
        let widths = WIDTHS.into_iter().zip(&self.widths).collect::<Vec<_>>();
        let descriptors = Slot::ALL.map(|s| (s, self.slots[s as usize]));
        tracing::info!(
            target: "victory_gpu::descriptor_demand",
            reason, frame, ?outcome, ?widths, ?descriptors,
            shader_cbv = ?self.shader_cbv,
            recorded_cbv = ?self.recorded_cbv,
            generation_compared = ?self.generation_compared,
            generation_changed = self.generation_changed,
            generation_untracked = ?self.generation_untracked,
            "rebuilt descriptor demand"
        );
        for composition in Composition::ALL {
            for (cbv, group) in self.kinds[composition as usize].iter().enumerate() {
                if group.total.tables == 0 {
                    continue;
                }
                tracing::info!(
                    target: "victory_gpu::descriptor_demand",
                    frame, ?composition, cbv = cbv != 0,
                    total = ?group.total, reserved = ?group.reserved,
                    exact_cross_list = ?group.cross_list,
                    exact_cross_list_reserved = ?group.cross_list_reserved,
                    exact_same_list = ?group.same_list,
                    duplicate_untracked = ?group.duplicate_untracked,
                    resolved_textures = ?group.resolved_textures,
                    generation_changed = group.generation_changed,
                    "rebuilt descriptor demand by composition"
                );
            }
        }
    }
}

static HISTORY: Mutex<Option<Report>> = Mutex::new(None);

pub(crate) fn present(frame: u64) {
    if (frame < 16 && frame.is_power_of_two() || frame.is_multiple_of(16)) && enabled() {
        log("present", frame);
    }
}

fn log(reason: &str, frame: u64) {
    let report = HISTORY.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(report) = report {
        report.log(reason, frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{
        HeapDesc, Placement, SHADER_VISIBLE, heap, record, release, tests::create,
    };
    use crate::object::S_OK;

    #[test]
    fn synthetic_descriptor_demand_reports_exact_cross_list_records() {
        let mut owned = std::ptr::null();
        assert_eq!(
            create(
                HeapDesc {
                    kind: VIEWS_KIND,
                    count: 8,
                    flags: SHADER_VISIBLE,
                    nodes: 0
                },
                Placement {
                    address: 0,
                    size: 0
                },
                &mut owned,
            ),
            S_OK
        );
        let heap = heap(owned);
        let first = heap.start;
        let second = first + 4 * 32;
        record(first, View::Constant(0x1000, 256));
        record(first + 32, View::Placed(false, [0; 14], [0; 16]));
        record(second, View::Constant(0x1000, 256));
        record(second + 32, View::Placed(false, [0; 14], [0; 16]));
        let mut report = Report::default();
        for (table, heap_id, list, outcome) in [
            (first, 1, 10, Outcome::LocalNew),
            (first, 1, 10, Outcome::LocalReuse),
            (second, 1, 20, Outcome::LocalNew),
            (second, 2, 30, Outcome::NoRange),
        ] {
            report.add(Request::new(heap, heap_id, list, table, 4, false), outcome);
        }
        let group = &report.kinds[Composition::Mixed as usize][1];
        assert_eq!(group.total.tables, 4);
        assert_eq!(group.same_list.slots, 4);
        assert_eq!(group.cross_list.slots, 4);
        assert_eq!(group.cross_list_reserved.slots, 4);
        assert_eq!(group.reserved.slots, 8);
        assert_eq!(report.slots[Slot::Cbv as usize], 4);
        assert_eq!(report.slots[Slot::PlacedSrv as usize], 4);
        assert_eq!(report.slots[Slot::Empty as usize], 8);

        // Replace the generation of a copied descriptor in the inspected
        // heap. The registry still has the old generation.
        // SAFETY: this word is in the first slot of the live test heap.
        unsafe {
            let generation = (first as *mut u32).add(2);
            generation.write_unaligned(generation.read_unaligned() + 1);
        }
        report.add(Request::new(heap, 1, 40, first, 4, true), Outcome::LocalNew);
        assert_eq!(report.slots[Slot::Stale as usize], 1);
        assert_eq!(report.generation_compared.tables, 2);
        assert_eq!(report.generation_changed, 1);
        assert_eq!(
            report.kinds[Composition::Unresolved as usize][1]
                .cross_list
                .tables,
            0
        );

        report.add(
            Request::new(heap, 1, 40, first, SCAN_LIMIT + 1, true),
            Outcome::NoRange,
        );
        assert_eq!(
            report.kinds[Composition::Uninspected as usize][1]
                .total
                .tables,
            1
        );
        assert_eq!(report.outcome[Outcome::NoRange as usize].tables, 2);
        assert!(Inspection::read(heap, first + 1, 1).is_none());
        assert!(Inspection::read(heap, second, 5).is_none());
        assert_eq!(release(owned), 0);
    }

    #[test]
    fn exact_history_keeps_known_keys_when_limits_are_reached() {
        let mut table_limit = History::<usize, usize>::default();
        for key in 0..HISTORY_TABLES {
            *table_limit.entry(key, 1).unwrap() = key;
        }
        assert!(table_limit.entry(HISTORY_TABLES, 1).is_none());
        assert_eq!(table_limit.entry(1, 1), Some(&mut 1));

        let mut slot_limit = History::<usize, usize>::default();
        let wide_tables = HISTORY_SLOTS / SCAN_LIMIT as usize;
        for key in 0..wide_tables {
            *slot_limit.entry(key, SCAN_LIMIT).unwrap() = key;
        }
        assert!(slot_limit.entry(wide_tables, 1).is_none());
        assert_eq!(slot_limit.entry(1, SCAN_LIMIT), Some(&mut 1));
    }
}
