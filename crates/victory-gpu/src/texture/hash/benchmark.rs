use super::{MIN_PARALLEL_BYTES, POOL, digest};
use crate::texture::{Staging, host};
use std::hint::black_box;
use std::sync::Barrier;
use std::time::{Duration, Instant};
use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, GetThreadTimes};
use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

struct Fixture {
    source: Vec<u8>,
    staging: Staging,
    tiled_tail: usize,
    linear_tail: usize,
}

impl Fixture {
    fn new(side: u32) -> Self {
        let layout = Layout::new(&Desc {
            dimension: Dimension::Tex2d,
            width: side,
            height: side,
            depth: 1,
            mips: 1,
            element_bytes: 4,
            block: 1,
            samples: 1,
            swizzle: Swizzle::SW_64KB_D,
        })
        .unwrap();
        let mut source = vec![0x37; layout.size as usize];
        let linear_bytes = u64::from(side) * u64::from(side) * 4;
        let (buffer, data) = host()
            .expect("native D3D12 test device")
            .upload_buffer(linear_bytes)
            .expect("a mapped upload buffer");
        let tiled_tail = layout.offset(0, side - 1, side - 1, 0).unwrap() as usize;
        let staging = Staging {
            buffer,
            #[cfg(feature = "diagnostics")]
            size: linear_bytes,
            data,
            footprints: vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 0,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    Width: side,
                    Height: side,
                    Depth: 1,
                    RowPitch: side * 4,
                },
            }],
            rows: vec![side],
            layout,
            mips: 1,
            memory: source.as_mut_ptr(),
            tracked: None,
            last: std::sync::Mutex::default(),
        };
        assert_eq!(staging.refresh_now(), (source.len(), true));
        let fixture = Self {
            source,
            staging,
            tiled_tail,
            linear_tail: linear_bytes as usize - 4,
        };
        fixture.verify();
        fixture
    }

    fn mutate(&mut self) {
        self.source[self.tiled_tail] ^= 0x80;
    }

    fn verify(&self) {
        let hash = self.staging.last.lock().unwrap().hash;
        assert_eq!(hash, Some(blake3::hash(&self.source)));
        // SAFETY: a live mapped upload buffer which only this fixture writes.
        let tail = unsafe { *self.staging.data.add(self.linear_tail) };
        assert_eq!(tail, self.source[self.tiled_tail]);
    }
}

#[derive(Clone, Copy, Default)]
struct Cpu {
    time: Duration,
    cycles: u64,
}

impl Cpu {
    fn since(self, before: Self) -> Self {
        Self {
            time: self.time - before.time,
            cycles: self.cycles - before.cycles,
        }
    }

    fn add(self, other: Self) -> Self {
        Self {
            time: self.time + other.time,
            cycles: self.cycles + other.cycles,
        }
    }
}

fn thread_cpu() -> Cpu {
    let [mut created, mut exited, mut kernel, mut user]: [FILETIME; 4] = Default::default();
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    let mut cycles = 0;
    // SAFETY: current thread pseudo-handle and live output locations.
    unsafe {
        let thread = GetCurrentThread();
        assert_ne!(
            GetThreadTimes(thread, &mut created, &mut exited, &mut kernel, &mut user),
            0
        );
        assert_ne!(QueryThreadCycleTime(thread, &mut cycles), 0);
    }
    Cpu {
        time: Duration::from_nanos(100 * (ticks(kernel) + ticks(user))),
        cycles,
    }
}

fn private_bytes() -> usize {
    let mut usage = PROCESS_MEMORY_COUNTERS_EX::default();
    // SAFETY: current process pseudo-handle and byte count of the EX structure.
    unsafe {
        assert_ne!(
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                (&raw mut usage).cast(),
                size_of_val(&usage) as u32
            ),
            0
        );
    }
    usage.PrivateUsage
}

fn worker_stack() -> (usize, usize) {
    fn query(at: usize) -> Option<MEMORY_BASIC_INFORMATION> {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: plain VirtualQuery read into the live output location.
        let size = unsafe { VirtualQuery(at as _, &mut info, size_of_val(&info)) };
        (size != 0).then_some(info)
    }
    let marker = 0u8;
    let base = query(black_box(&marker) as *const u8 as usize)
        .unwrap()
        .AllocationBase as usize;
    let mut at = base;
    let mut commit = 0;
    while let Some(region) = query(at) {
        if region.AllocationBase as usize != base {
            break;
        }
        if region.State == MEM_COMMIT {
            commit += region.RegionSize;
        }
        at += region.RegionSize;
    }
    (at - base, commit)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn report_case(side: u32, callers: usize, changed: bool) {
    const WARMUP: usize = 3;
    const SAMPLES: usize = 21;
    let fixtures: Vec<_> = (0..callers).map(|_| Fixture::new(side)).collect();
    let bytes = fixtures[0].source.len();
    let rendezvous = Barrier::new(callers);
    let pool = POOL.get().and_then(Option::as_ref);
    let warmed = Barrier::new(callers + 1);
    let start_measurement = Barrier::new(callers + 1);
    let (results, worker_before) = std::thread::scope(|scope| {
        let handles: Vec<_> = fixtures
            .into_iter()
            .map(|mut fixture| {
                let (rendezvous, warmed, start_measurement) =
                    (&rendezvous, &warmed, &start_measurement);
                scope.spawn(move || {
                    let mut correct = true;
                    for _ in 0..WARMUP {
                        if changed {
                            fixture.mutate();
                        }
                        correct &= fixture.staging.refresh_now() == (bytes, changed);
                    }
                    // The coordinator samples worker CPU before releasing callers.
                    warmed.wait();
                    start_measurement.wait();
                    let before = thread_cpu();
                    let mut samples = [Duration::ZERO; SAMPLES];
                    for sample in &mut samples {
                        if changed {
                            fixture.mutate();
                        }
                        rendezvous.wait();
                        let start = Instant::now();
                        let result = black_box(fixture.staging.refresh_now());
                        *sample = start.elapsed();
                        correct &= result == (bytes, changed);
                    }
                    let cpu = thread_cpu().since(before);
                    rendezvous.wait();
                    assert!(
                        correct,
                        "refresh outcome for {bytes} bytes, changed={changed}"
                    );
                    fixture.verify();
                    (samples, cpu)
                })
            })
            .collect();
        warmed.wait();
        let worker_before = pool.map(|pool| pool.broadcast(|_| thread_cpu()));
        start_measurement.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        (results, worker_before)
    });
    let worker_cpu = pool
        .zip(worker_before)
        .map_or(Cpu::default(), |(pool, before)| {
            pool.broadcast(|_| thread_cpu())
                .into_iter()
                .zip(before)
                .map(|(after, before)| after.since(before))
                .fold(Cpu::default(), Cpu::add)
        });
    let caller_cpu = results
        .iter()
        .map(|(_, cpu)| *cpu)
        .fold(Cpu::default(), Cpu::add);
    let mut critical: Vec<_> = (0..SAMPLES)
        .map(|i| results.iter().map(|(wall, _)| wall[i]).max().unwrap())
        .collect();
    critical.sort_unstable();
    eprintln!(
        "refresh bytes={bytes} callers={callers} changed={changed} wall_median_ms={:.3}          wall_max_ms={:.3} caller_cpu_ms={:.3} worker_cpu_ms={:.3} caller_cycles={} worker_cycles={}",
        ms(critical[SAMPLES / 2]),
        ms(critical[SAMPLES - 1]),
        ms(caller_cpu.time),
        ms(worker_cpu.time),
        caller_cpu.cycles,
        worker_cpu.cycles
    );
}

#[test]
#[ignore = "native D3D12 upload buffers and optimized CPU timing"]
fn report_untracked_refresh_and_pool_cost() {
    assert!(!cfg!(debug_assertions), "build with --release");
    let sample = vec![0x37; MIN_PARALLEL_BYTES];
    let expected = blake3::hash(&sample);
    let before = private_bytes();
    let start = Instant::now();
    let processed = digest(black_box(&sample));
    let first = start.elapsed();
    let after = private_bytes();
    assert_eq!(processed, expected);
    let pool = POOL.get().and_then(Option::as_ref);
    let stacks = || pool.map_or_else(Vec::new, |pool| pool.broadcast(|_| worker_stack()));
    eprintln!(
        "pool_active={} first_hash_ms={:.3} private_before={before} private_after={after} stacks_reserve_commit={:?}",
        pool.is_some(),
        ms(first),
        stacks()
    );
    for (side, callers) in [
        (1024, 1),
        (1536, 1),
        (2048, 1),
        (4096, 1),
        (2048, 2),
        (2048, 4),
    ] {
        for changed in [false, true] {
            report_case(side, callers, changed);
        }
    }
    eprintln!("final_stacks_reserve_commit={:?}", stacks());
}
