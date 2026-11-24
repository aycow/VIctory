use std::sync::OnceLock;

const MIN_PARALLEL_BYTES: usize = 4 << 20;
const WORKERS: usize = 2;
static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();

#[cfg(test)]
mod benchmark;

pub(super) fn digest(bytes: &[u8]) -> blake3::Hash {
    if bytes.len() >= MIN_PARALLEL_BYTES
        && let Some(pool) = POOL.get_or_init(|| {
            let enabled = std::env::var_os("VICTORY_PARALLEL_TEXTURE_HASH").is_some();
            let enough_cores = std::thread::available_parallelism().is_ok_and(|n| n.get() >= 4);
            (enabled && enough_cores).then(new_pool).flatten()
        })
    {
        return parallel_digest(pool, bytes);
    }
    blake3::hash(bytes)
}

fn new_pool() -> Option<rayon::ThreadPool> {
    let builder = rayon::ThreadPoolBuilder::new();
    #[cfg(test)]
    let builder = match std::env::var("VICTORY_TEST_HASH_STACK_KIB") {
        Ok(kib) => builder.stack_size(kib.parse::<usize>().unwrap() << 10),
        Err(_) => builder,
    };
    builder
        .num_threads(WORKERS)
        .thread_name(|i| format!("victory-texture-hash-{i}"))
        .build()
        .ok()
}

fn parallel_digest(pool: &rayon::ThreadPool, bytes: &[u8]) -> blake3::Hash {
    pool.install(|| blake3::Hasher::new().update_rayon(bytes).finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    #[test]
    fn two_workers_preserve_the_serial_digest() {
        let pool = new_pool().unwrap();
        for size in [0, 1, 1023, 1024, 1025, MIN_PARALLEL_BYTES, MIN_PARALLEL_BYTES + 771] {
            let mut input = vec![0x2e; size];
            let expected = blake3::hash(&input);
            assert_eq!(parallel_digest(&pool, &input), expected);
            if let Some(tail) = input.last_mut() {
                *tail ^= 0x80;
                let changed = parallel_digest(&pool, &input);
                assert_ne!(changed, expected);
                assert_eq!(changed, blake3::hash(&input));
            }
        }
    }

    #[test]
    #[ignore = "requires a native D3D12 upload buffer"]
    fn untracked_texture_skips_equal_bytes_and_uploads_a_changed_last_texel() {
        use crate::texture::{Staging, host};
        use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;

        let layout = Layout::new(&Desc {
            dimension: Dimension::Tex2d,
            width: 1024,
            height: 1024,
            depth: 1,
            mips: 1,
            element_bytes: 4,
            block: 1,
            samples: 1,
            swizzle: Swizzle::SW_64KB_D,
        })
        .unwrap();
        let size = layout.size as usize;
        assert!(size >= MIN_PARALLEL_BYTES);
        let mut source = vec![0x37; size];
        let memory = source.as_mut_ptr();
        let (buffer, data) = host().expect("a native D3D12 test device").upload_buffer(size as u64).unwrap();
        let last = layout.offset(0, 1023, 1023, 0).unwrap() as usize;
        let staging = Staging {
            buffer,
            #[cfg(feature = "diagnostics")]
            size: size as u64,
            data,
            footprints: vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 0,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    Width: 1024,
                    Height: 1024,
                    Depth: 1,
                    RowPitch: 4096,
                },
            }],
            rows: vec![1024],
            layout,
            mips: 1,
            memory,
            tracked: None,
            last: std::sync::Mutex::default(),
        };
        assert_eq!(staging.refresh_now(), (size, true));
        assert_eq!(staging.refresh_now(), (size, false));
        // SAFETY: the fixture owns the source and upload and no GPU commands run.
        unsafe {
            assert_eq!(*data.add(size - 4), 0x37);
            *memory.add(last) = 0x69;
        }
        assert_eq!(staging.refresh_now(), (size, true));
        assert_eq!(staging.refresh_now(), (size, false));
        // SAFETY: the last texel is within the initialized upload.
        assert_eq!(unsafe { *data.add(size - 4) }, 0x69);
        assert_eq!(staging.last.lock().unwrap().hash, Some(blake3::hash(&source)));
    }

    #[test]
    #[ignore = "collect paired wall-time distributions in a release build"]
    fn compare_serial_and_parallel_hash_cost() {
        let pool = new_pool().unwrap();
        let timed = |hash: &dyn Fn() -> blake3::Hash| {
            let start = Instant::now();
            let result = black_box(hash());
            (start.elapsed(), result)
        };
        for size in [1_048_576, 8_847_360, 33_423_360, 133_693_440] {
            let input: Vec<u8> = (0..size).map(|i: usize| i.wrapping_mul(97) as u8).collect();
            let a = || blake3::hash(black_box(&input));
            let b = || parallel_digest(&pool, black_box(&input));
            for _ in 0..3 {
                assert_eq!(a(), b());
            }
            let mut serial = [Duration::ZERO; 21];
            let mut parallel = [Duration::ZERO; 21];
            for i in 0..serial.len() {
                let (s, p) = if i % 2 == 0 {
                    (timed(&a), timed(&b))
                } else {
                    let p = timed(&b);
                    (timed(&a), p)
                };
                assert_eq!(s.1, p.1);
                (serial[i], parallel[i]) = (s.0, p.0);
            }
            serial.sort_unstable();
            parallel.sort_unstable();
            let ms = |d: Duration| d.as_secs_f64() * 1e3;
            eprintln!(
                "{size} bytes: serial median {:.3} max {:.3} ms, parallel median {:.3} max {:.3} ms",
                ms(serial[10]), ms(serial[20]), ms(parallel[10]), ms(parallel[20]),
            );
        }
    }
}
