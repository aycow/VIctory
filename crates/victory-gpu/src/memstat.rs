//! Optional capacity and process-commit samples, including during loading.

use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering::Relaxed},
};

use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::Win32::Graphics::Dxgi::*;

#[derive(Default)]
pub(crate) struct Gauge {
    bytes: AtomicU64,
    peak: AtomicU64,
}

impl Gauge {
    fn sample(&self) -> (u64, u64) {
        let bytes = self.bytes.load(Relaxed);
        (bytes, self.peak.load(Relaxed).max(bytes))
    }
}

pub(crate) static SHADOW: Gauge = Gauge {
    bytes: AtomicU64::new(0),
    peak: AtomicU64::new(0),
};
pub(crate) static SNAPSHOT: Gauge = Gauge {
    bytes: AtomicU64::new(0),
    peak: AtomicU64::new(0),
};
pub(crate) static SCRATCH: Gauge = Gauge {
    bytes: AtomicU64::new(0),
    peak: AtomicU64::new(0),
};
pub(crate) static UPLOAD: Gauge = Gauge {
    bytes: AtomicU64::new(0),
    peak: AtomicU64::new(0),
};
pub(crate) static TEXTURE_UPLOAD: Gauge = Gauge {
    bytes: AtomicU64::new(0),
    peak: AtomicU64::new(0),
};
static TEXTURE_READ: Gauge = Gauge {
    bytes: AtomicU64::new(0),
    peak: AtomicU64::new(0),
};

/// Records the capacity of the process-wide CPU detile buffer under its lock.
pub(crate) fn set_texture_read_capacity(bytes: u64) {
    if enabled() {
        TEXTURE_READ.peak.fetch_max(bytes, Relaxed);
        TEXTURE_READ.bytes.store(bytes, Relaxed);
    }
}

pub(crate) fn texture_read_capacity() -> (u64, u64) {
    TEXTURE_READ.sample()
}

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("VICTORY_MEMORY_STATS").is_some())
}

/// Capacity owned by a resource or allocation pool for its lifetime.
#[derive(Default)]
pub(crate) struct Charge(Option<(&'static Gauge, u64)>);

impl Charge {
    pub(crate) fn new(gauge: &'static Gauge, bytes: u64) -> Self {
        let mut charge = Self::default();
        charge.add(gauge, bytes);
        charge
    }

    pub(crate) fn add(&mut self, gauge: &'static Gauge, bytes: u64) {
        if enabled() {
            self.add_enabled(gauge, bytes);
        }
    }

    fn add_enabled(&mut self, gauge: &'static Gauge, bytes: u64) {
        let (owner, total) = self.0.get_or_insert((gauge, 0));
        debug_assert!(std::ptr::eq(*owner, gauge));
        *total += bytes;
        gauge
            .peak
            .fetch_max(gauge.bytes.fetch_add(bytes, Relaxed) + bytes, Relaxed);
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if let Some((gauge, bytes)) = self.0 {
            gauge.bytes.fetch_sub(bytes, Relaxed);
        }
    }
}

/// Starts sampling on its own cadence so loading before the first present
/// also has samples. Only the adapter LUID is passed to the sampler.
pub(crate) fn start(device: &ID3D12Device) {
    if !enabled() {
        return;
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        // SAFETY: the device is live, and the descriptor contains no pointers.
        let luid = unsafe { device.GetAdapterLuid() };
        if let Err(error) = std::thread::Builder::new()
            .name("victory-memory".into())
            .spawn(move || sample_loop(luid))
        {
            tracing::warn!(%error, "could not start memory samples");
        }
    });
}

fn sample_loop(luid: LUID) {
    // SAFETY: the factory and adapter are both owned by this thread.
    let adapter = unsafe {
        CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|factory| factory.EnumAdapterByLuid::<IDXGIAdapter3>(luid))
            .ok()
    };
    let since = std::time::Instant::now();
    loop {
        sample(since.elapsed().as_secs(), adapter.as_ref());
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

fn commit() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
    let mut info = PERFORMANCE_INFORMATION::default();
    let bytes = size_of_val(&info) as u32;
    info.cb = bytes;
    // SAFETY: info is a valid output buffer of the given size.
    if unsafe { GetPerformanceInfo(&mut info, bytes) } == 0 {
        return None;
    }
    Some((
        (info.CommitTotal * info.PageSize) as u64,
        (info.CommitLimit * info.PageSize) as u64,
    ))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ShadowRanges {
    pub(crate) count: usize,
    pub(crate) described_bytes: u64,
    pub(crate) unique_virtual_bytes: u64,
}

impl ShadowRanges {
    pub(crate) fn from_extents(mut extents: Vec<(u64, u64)>) -> Self {
        extents.sort_unstable();
        let mut ranges = Self::default();
        let mut accounted_until = 0;
        for (start, end) in extents {
            ranges.count += 1;
            ranges.described_bytes += end - start;
            ranges.unique_virtual_bytes += end.saturating_sub(start.max(accounted_until));
            accounted_until = accounted_until.max(end);
        }
        ranges
    }
}

fn sample(elapsed_s: u64, adapter: Option<&IDXGIAdapter3>) {
    let mem = crate::copystat::process_memory();
    let commit = commit();
    let budget = |group| {
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: a live DXGI adapter, its primary node and a sized output buffer.
        unsafe { adapter?.QueryVideoMemoryInfo(0, group, &mut info) }.ok()?;
        Some(info)
    };
    let local = budget(DXGI_MEMORY_SEGMENT_GROUP_LOCAL);
    let non_local = budget(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL);
    let (shadow_bytes, shadow_peak) = SHADOW.sample();
    let (snapshot_bytes, snapshot_peak) = SNAPSHOT.sample();
    let (list_scratch_bytes, list_scratch_peak) = SCRATCH.sample();
    let (list_upload_bytes, list_upload_peak) = UPLOAD.sample();
    let (texture_upload_bytes, texture_upload_peak) = TEXTURE_UPLOAD.sample();
    let (texture_read_bytes, texture_read_peak) = texture_read_capacity();
    let (native_textures, native_texture_bytes, native_used_bytes, _, _, _) =
        crate::resource::texture_counts();
    let shadow_ranges = crate::shadow::cached_shadow_ranges();
    tracing::info!(
        target: "victory_gpu::memory", frame = crate::frame::current(), elapsed_s,
        private_bytes = ?mem.as_ref().map(|m| m.private_usage),
        working_set_bytes = ?mem.as_ref().map(|m| m.working_set),
        faults_total = ?mem.as_ref().map(|m| m.page_faults),
        system_commit_bytes = ?commit.map(|m| m.0), system_commit_limit = ?commit.map(|m| m.1),
        gpu_local_usage = ?local.map(|m| m.CurrentUsage), gpu_local_budget = ?local.map(|m| m.Budget),
        gpu_non_local_usage = ?non_local.map(|m| m.CurrentUsage),
        shadow_bytes, shadow_peak, snapshot_bytes, snapshot_peak,
        cached_shadows = ?shadow_ranges.as_ref().map(|r| r.count),
        cached_shadow_guest_bytes = ?shadow_ranges.as_ref().map(|r| r.described_bytes),
        cached_shadow_guest_unique_bytes = ?shadow_ranges.as_ref().map(|r| r.unique_virtual_bytes),
        list_scratch_bytes, list_scratch_peak, list_upload_bytes, list_upload_peak,
        texture_upload_bytes, texture_upload_peak, texture_read_bytes, texture_read_peak,
        idle_snapshot_bytes = ?crate::shadow::idle_snapshot_bytes(),
        "GPU bridge capacity"
    );
    tracing::info!(
        target: "victory_gpu::memory", frame = crate::frame::current(), elapsed_s,
        native_textures, native_texture_bytes, native_used_bytes,
        "native host texture capacity"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_shadow_extents_count_overlap_and_ignore_gaps() {
        let extents = vec![(40, 50), (18, 27), (10, 26), (10, 12), (27, 30)];
        assert_eq!(
            ShadowRanges::from_extents(extents),
            ShadowRanges {
                count: 5,
                described_bytes: 40,
                unique_virtual_bytes: 30,
            }
        );
        assert_eq!(ShadowRanges::from_extents(Vec::new()), ShadowRanges::default());
    }

    #[test]
    fn simultaneous_owners_contribute_to_peak_and_release_capacity() {
        static TEST: Gauge = Gauge {
            bytes: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        };
        let mut old = Charge::default();
        old.add_enabled(&TEST, 2 << 20);
        let mut grown = Charge::default();
        grown.add_enabled(&TEST, 4 << 20);
        assert_eq!(TEST.sample(), (6 << 20, 6 << 20));
        drop(old);
        assert_eq!(TEST.sample(), (4 << 20, 6 << 20));
        drop(grown);
        assert_eq!(TEST.sample(), (0, 6 << 20));
    }

    #[test]
    #[ignore = "requires a Windows D3D12 device and VICTORY_MEMORY_STATS=1"]
    fn observes_shadow_and_list_capacity_through_reset_and_release() {
        assert!(enabled());
        const MIB: u64 = 1 << 20;
        let host = crate::host::host().expect("D3D12 device");
        let title = vec![19u8; (4 * MIB) as usize];
        let address = title.as_ptr() as u64;
        crate::shadow::register(address as usize, address, title.len() as u64);
        let shadow = crate::shadow::find(address, title.len() as u64).unwrap();
        assert_eq!(SHADOW.sample(), (4 * MIB, 4 * MIB));
        let (view, own) = crate::shadow::for_view(address + 4, 456, 456).unwrap();
        assert!(own);
        // SAFETY: a read-only description of our live exact-view buffer.
        let view_width = unsafe { view.buffer.GetDesc() }.Width;
        let charged = SHADOW.sample().0 - 4 * MIB;
        eprintln!("exact-view buffer width {view_width}, charged {charged}");
        assert_eq!(view_width, 512);
        assert_eq!(charged, view_width);
        let extents = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            loop {
                if let Some(ranges) = crate::shadow::cached_shadow_ranges() {
                    break ranges;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "cached shadow sample remained busy"
                );
                std::thread::yield_now();
            }
        };
        assert_eq!(
            extents(),
            ShadowRanges {
                count: 2,
                described_bytes: 4 * MIB + 456,
                unique_virtual_bytes: 4 * MIB,
            }
        );
        drop(view);
        assert_eq!(
            extents(),
            ShadowRanges {
                count: 1,
                described_bytes: 4 * MIB,
                unique_virtual_bytes: 4 * MIB,
            }
        );
        assert_eq!(SHADOW.sample(), (4 * MIB, 4 * MIB + view_width));
        assert_eq!(shadow.update(address as usize, title.len()), title.len());
        shadow.writing(address as usize, (2 * MIB) as usize);
        assert_eq!(SNAPSHOT.sample(), (2 * MIB, 2 * MIB));
        shadow.writing((address + 2 * MIB) as usize, (2 * MIB) as usize);
        assert_eq!(SNAPSHOT.sample(), (4 * MIB, 6 * MIB));
        shadow.copy_back(address as usize, (2 * MIB) as usize);
        shadow.copy_back((address + 2 * MIB) as usize, (2 * MIB) as usize);
        assert_eq!(SNAPSHOT.sample(), (0, 6 * MIB));
        assert_eq!(crate::shadow::idle_snapshot_bytes(), Some(6 << 20));

        let list = host.new_list().unwrap();
        list.scratch(256).unwrap();
        list.scratch(5 * MIB).unwrap();
        assert_eq!(SCRATCH.sample(), (4 * MIB, 4 * MIB));
        assert_eq!(UPLOAD.sample(), (5 * MIB, 5 * MIB));
        list.close();
        list.reset();
        assert_eq!(SCRATCH.sample(), (4 * MIB, 4 * MIB));
        assert_eq!(UPLOAD.sample(), (0, 5 * MIB));
        drop(list);
        assert_eq!(SCRATCH.sample(), (0, 4 * MIB));
        crate::shadow::unregister(address as usize, address);
        drop(shadow);
        assert_eq!(SHADOW.sample(), (0, 4 * MIB + view_width));
        let process = crate::copystat::process_memory().unwrap();
        let (system, limit) = commit().unwrap();
        assert!(process.private_usage > 0 && system <= limit);
        eprintln!(
            "peak shadow 4 MiB + {view_width} B, peak active snapshot 6 MiB (4 MiB final live), idle snapshots 6 MiB, peak scratch 4 MiB, peak deferred upload 5 MiB; process private {}, system commit {system}/{limit}",
            process.private_usage
        );
    }
}
