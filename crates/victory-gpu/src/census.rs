//! Aggregated GPU completion timestamps. Readback uses the submission's
//! existing fence, so collection adds no queue submission or wait.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;

const STAMPS: u32 = 4096;
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("VICTORY_GPU_CENSUS").is_some())
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct Pipeline {
    pub(crate) hash: String,
    pub(crate) shaders: Vec<(&'static str, String)>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Work {
    Dispatch([u32; 3]),
    Mesh([u32; 3]),
    Draw(u32, u32),
    Indexed(u32, u32),
    Indirect {
        kind: u32,
        limit: u32,
        stride: u32,
        counted: bool,
    },
    Clamp(u32),
}

impl Work {
    fn kind(self) -> &'static str {
        match self {
            Self::Dispatch(_) => "compute",
            Self::Mesh(_) => "mesh",
            Self::Draw(..) => "draw",
            Self::Indexed(..) => "draw_indexed",
            Self::Indirect { kind: 0, .. } => "indirect_draw",
            Self::Indirect { kind: 1, .. } => "indirect_draw_indexed",
            Self::Indirect { kind: 2, .. } => "indirect_compute",
            Self::Indirect { .. } => "indirect_mesh",
            Self::Clamp(_) => "indirect_clamp",
        }
    }

    fn dimensions(self) -> [u32; 3] {
        match self {
            Self::Dispatch(groups) | Self::Mesh(groups) => groups,
            Self::Draw(count, instances) | Self::Indexed(count, instances) => [count, instances, 1],
            Self::Indirect { limit, stride, .. } => [limit, stride, 0],
            Self::Clamp(records) => [records.div_ceil(64), 1, 1],
        }
    }
}

struct Operation {
    pipeline: Arc<Pipeline>,
    work: Work,
}

#[derive(Default)]
pub(crate) struct List {
    recording: Mutex<Arc<Recording>>,
}

/// The operation identities and their native query storage have one lifetime.
#[derive(Default)]
pub(crate) struct Recording {
    queries: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
    operations: Vec<Operation>,
}

pub(crate) struct Sample<'a> {
    list: &'a ID3D12GraphicsCommandList,
    heap: ID3D12QueryHeap,
    end: u32,
}

impl Drop for Sample<'_> {
    fn drop(&mut self) {
        // SAFETY: the caller records the operation while the list is open.
        unsafe {
            self.list
                .EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, self.end)
        };
    }
}

impl List {
    pub(crate) fn begin<'a>(
        &'a self,
        list: &'a ID3D12GraphicsCommandList,
        pipeline: &Arc<Pipeline>,
        work: Work,
    ) -> Option<Sample<'a>> {
        let mut active = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        let recording = Arc::get_mut(&mut active).expect("census recording still submitted");
        let index = recording.operations.len();
        recording.operations.push(Operation {
            pipeline: pipeline.clone(),
            work,
        });
        if index >= STAMPS as usize / 2 {
            return None;
        }
        let (heap, _) = recording.queries.get_or_init(|| {
            let mut device: Option<ID3D12Device> = None;
            // SAFETY: the list is live and the result pointer is ours.
            let buffers = unsafe { list.GetDevice(&mut device) }
                .ok()
                .and_then(|()| super::host::timestamp_buffers(&device?, STAMPS));
            if buffers.is_none() {
                tracing::warn!(target: "victory_gpu::census", "cannot allocate GPU census queries");
            }
            buffers
        }).as_ref()?;
        let start = index as u32 * 2;
        // SAFETY: an open list and a slot in its own timestamp heap.
        unsafe { list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, start) };
        Some(Sample {
            list,
            heap: heap.clone(),
            end: start + 1,
        })
    }

    pub(crate) fn close(&self, list: &ID3D12GraphicsCommandList) {
        let recording = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(Some((heap, back))) = recording.queries.get() {
            let count = recording.operations.len();
            let stamps = count.min(STAMPS as usize / 2) as u32 * 2;
            if stamps != 0 {
                // SAFETY: an open list, initialized queries, and its own readback.
                unsafe {
                    list.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, stamps, back, 0)
                };
            }
        }
    }

    pub(crate) fn reset(&self) {
        let mut active = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(recording) = Arc::get_mut(&mut active) {
            recording.operations.clear();
        } else {
            *active = Arc::new(Recording::default());
        }
    }

    pub(crate) fn snapshot(&self) -> Option<Arc<Recording>> {
        let recording = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        (!recording.operations.is_empty()).then(|| recording.clone())
    }
}

impl Recording {
    pub(crate) fn collect(&self, totals: &mut Totals, frequency: Option<u64>) {
        let operations = &self.operations;
        if operations.is_empty() {
            return;
        }
        let read = self.queries.get().and_then(Option::as_ref).zip(frequency);
        let mut data = std::ptr::null_mut();
        let count = operations.len().min(STAMPS as usize / 2) * 2;
        let stamps = read.filter(|((_, back), _)| {
            let range = D3D12_RANGE {
                Begin: 0,
                End: count * size_of::<u64>(),
            };
            // SAFETY: the host supplied a frequency only after this batch's native fence completed.
            unsafe { back.Map(0, Some(&range), Some(&mut data)).is_ok() }
        });
        // SAFETY: a successful map covers the timestamp pairs resolved by this list.
        let ticks: &[u64] = stamps.map_or(&[], |_| unsafe {
            std::slice::from_raw_parts(data.cast::<u64>(), count)
        });
        for (index, op) in operations.iter().enumerate() {
            let ns = ticks.get(index * 2..index * 2 + 2).and_then(|pair| {
                let delta = pair[1].checked_sub(pair[0])?;
                u64::try_from(u128::from(delta) * 1_000_000_000 / u128::from(frequency?)).ok()
            });
            totals
                .execution
                .entry((op.pipeline.clone(), op.work.kind()))
                .or_default()
                .add(op.work, ns);
        }
        if let Some(((_, back), _)) = stamps {
            // SAFETY: close the successful map without indicating a CPU write.
            unsafe { back.Unmap(0, Some(&D3D12_RANGE::default())) };
        }
    }
}

#[derive(Default)]
struct Samples {
    ns: Vec<u64>,
    total_ns: u128,
}

impl Samples {
    fn add(&mut self, ns: u64) {
        self.ns.push(ns);
        self.total_ns += u128::from(ns);
    }

    fn include(&mut self, other: &Self) {
        self.ns.extend_from_slice(&other.ns);
        self.total_ns += other.total_ns;
    }

    fn summary(mut self) -> (usize, f64, f64, f64) {
        let count = self.ns.len();
        self.ns.sort_unstable();
        let median_ns = if count == 0 {
            0.0
        } else {
            (self.ns[(count - 1) / 2] as f64 + self.ns[count / 2] as f64) / 2.0
        };
        let max_ns = self.ns.last().copied().unwrap_or_default();
        (
            count,
            median_ns / 1e6,
            max_ns as f64 / 1e6,
            self.total_ns as f64 / 1e6,
        )
    }
}

#[derive(Default)]
struct Execution {
    calls: u64,
    max_dimensions: [u32; 3],
    total_items: u128,
    max_items: u128,
    fixed_indirect_records: u64,
    counted_indirect_calls: u64,
    counted_indirect_limit: u64,
    timings: Samples,
}

impl Execution {
    fn add(&mut self, work: Work, ns: Option<u64>) {
        self.calls += 1;
        let dimensions = work.dimensions();
        for (high, value) in self.max_dimensions.iter_mut().zip(dimensions) {
            *high = (*high).max(value);
        }
        let items = match work {
            Work::Indirect { limit, counted, .. } => {
                if counted {
                    self.counted_indirect_calls += 1;
                    self.counted_indirect_limit += u64::from(limit);
                } else {
                    self.fixed_indirect_records += u64::from(limit);
                }
                u128::from(limit)
            }
            Work::Clamp(records) => u128::from(records),
            _ => dimensions.into_iter().map(u128::from).product(),
        };
        self.total_items += items;
        self.max_items = self.max_items.max(items);
        if let Some(ns) = ns {
            self.timings.add(ns);
        }
    }
}

#[derive(Default)]
struct Creation {
    failed: usize,
    timings: Samples,
}

#[derive(Default)]
pub(crate) struct Totals {
    execution: BTreeMap<(Arc<Pipeline>, &'static str), Execution>,
    creation: BTreeMap<Arc<Pipeline>, Creation>,
    refusals: BTreeMap<String, u64>,
    submissions: u64,
    first_present: u64,
    last_present: u64,
    since: Option<Instant>,
}

static TOTALS: Mutex<Option<Totals>> = Mutex::new(None);

pub(crate) fn with_totals<R>(record: impl FnOnce(&mut Totals) -> R) -> R {
    let mut state = TOTALS.lock().unwrap_or_else(|e| e.into_inner());
    let totals = state.get_or_insert_with(Totals::default);
    if totals.since.is_none() {
        totals.since = Some(Instant::now());
        totals.first_present = crate::queue::presented();
    }
    record(totals)
}

pub(crate) fn created(pipeline: Arc<Pipeline>, elapsed: Duration, failed: bool) {
    with_totals(|totals| {
        let entry = totals.creation.entry(pipeline).or_default();
        entry.failed += usize::from(failed);
        entry.timings.add(elapsed.as_nanos() as u64);
    });
}

pub(crate) fn refused(reason: &str) {
    if enabled() {
        with_totals(|totals| {
            *totals.refusals.entry(reason.to_owned()).or_default() += 1;
        });
    }
}

/// Finish pending work at normal title shutdown before reporting the last window.
pub fn flush_after_drain() {
    if enabled() && let Some(host) = super::host::made() {
        host.drain();
    }
    flush();
}

/// Writes any completed samples since the last periodic report.
pub fn flush() {
    if enabled() {
        let report = TOTALS.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(report) = report {
            report.finish().report();
        }
    }
}

impl Totals {
    pub(crate) fn submitted(&mut self) -> Option<Self> {
        self.submissions += 1;
        (self.since?.elapsed() >= REPORT_INTERVAL).then(|| {
            let report = std::mem::take(self).finish();
            self.since = Some(Instant::now());
            self.first_present = report.last_present;
            report
        })
    }

    fn finish(mut self) -> Self {
        self.last_present = crate::queue::presented();
        self
    }

    pub(crate) fn report(self) {
        if self.submissions == 0 && self.creation.is_empty() && self.refusals.is_empty() {
            return;
        }
        let seconds = self
            .since
            .map_or(0.0, |start| start.elapsed().as_secs_f64());
        tracing::info!(
            target: "victory_gpu::census",
            seconds, submissions = self.submissions, presents_at_start = self.first_present,
            presents_at_end = self.last_present, execution_rows = self.execution.len(),
            creation_rows = self.creation.len(), refusal_rows = self.refusals.len(), "GPU census window"
        );
        let calls_per_second = |calls| {
            if seconds > 0.0 {
                calls as f64 / seconds
            } else {
                0.0
            }
        };
        let mut rows: Vec<_> = self.execution.into_iter().collect();
        rows.sort_by_key(|(_, entry)| Reverse(entry.timings.total_ns));
        let mut by_shader = BTreeMap::new();
        for ((pipeline, kind), entry) in rows {
            for (stage, hash) in &pipeline.shaders {
                let (calls, timings): &mut (u64, Samples) =
                    by_shader.entry((*stage, hash.clone(), kind)).or_default();
                *calls += entry.calls;
                timings.include(&entry.timings);
            }
            let (timed, median_ms, max_ms, accumulated_ms) = entry.timings.summary();
            tracing::info!(
                target: "victory_gpu::census",
                pipeline = %pipeline.hash, shaders = ?pipeline.shaders, kind,
                calls = entry.calls, calls_per_second = calls_per_second(entry.calls),
                timed, median_ms, max_ms, accumulated_ms,
                max_dimensions = ?entry.max_dimensions, total_items = entry.total_items,
                max_items = entry.max_items, fixed_indirect_records = entry.fixed_indirect_records,
                counted_indirect_calls = entry.counted_indirect_calls,
                counted_indirect_limit = entry.counted_indirect_limit, "GPU completion spans"
            );
        }
        let mut shader_rows: Vec<_> = by_shader.into_iter().collect();
        shader_rows.sort_by_key(|(_, (_, samples))| Reverse(samples.total_ns));
        for ((stage, shader, kind), (calls, samples)) in shader_rows {
            let (timed, median_ms, max_ms, accumulated_ms) = samples.summary();
            tracing::info!(
                target: "victory_gpu::census",
                stage, %shader, kind, calls, calls_per_second = calls_per_second(calls),
                timed, median_ms, max_ms, accumulated_ms,
                "GPU completion spans for pipelines using shader"
            );
        }
        let mut refusals: Vec<_> = self.refusals.into_iter().collect();
        refusals.sort_by_key(|(_, count)| Reverse(*count));
        for (reason, calls) in refusals {
            tracing::info!(target: "victory_gpu::census", %reason, calls, "GPU command refusals");
        }
        let mut rows: Vec<_> = self.creation.into_iter().collect();
        rows.sort_by_key(|(_, entry)| Reverse(entry.timings.total_ns));
        for (pipeline, entry) in rows {
            let (attempts, median_ms, max_ms, accumulated_ms) = entry.timings.summary();
            tracing::info!(
                target: "victory_gpu::census",
                pipeline = %pipeline.hash, shaders = ?pipeline.shaders, attempts,
                failed = entry.failed, median_ms, max_ms, accumulated_ms, "CPU pipeline build spans"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_timed_and_untimed_calls_and_indirect_limits() {
        let mut direct = Execution::default();
        for (dimensions, ns) in [
            ([3, 2, 1], Some(8_000_000)),
            ([1, 3, 2], Some(2_000_000)),
            ([4, 4, 1], None),
        ] {
            direct.add(Work::Dispatch(dimensions), ns);
        }
        assert_eq!(direct.calls, 3);
        assert_eq!(direct.total_items, 28);
        assert_eq!(direct.max_items, 16);
        assert_eq!(direct.max_dimensions, [4, 4, 2]);
        assert_eq!(direct.timings.summary(), (2, 5.0, 8.0, 10.0));

        let mut indirect = Execution::default();
        for (limit, counted) in [(7, false), (13, true), (2, true)] {
            indirect.add(
                Work::Indirect {
                    kind: 2,
                    limit,
                    stride: 12,
                    counted,
                },
                None,
            );
        }
        assert_eq!(indirect.calls, 3);
        assert_eq!(indirect.fixed_indirect_records, 7);
        assert_eq!(indirect.counted_indirect_calls, 2);
        assert_eq!(indirect.counted_indirect_limit, 15);
        assert_eq!(indirect.timings.summary(), (0, 0.0, 0.0, 0.0));
    }
}

#[cfg(test)]
mod gpu_tests;
