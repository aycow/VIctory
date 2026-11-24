use super::*;

const NODES: usize = 1 << 16;
const GUEST_READ: u64 = 0x10_ffff_f000;
const GUEST_WRITE: u64 = 0x20_ffff_e000;
const SENTINEL: u32 = 0xdead_baad;

const WINDOW_SHADER: &str = r#"
#define RS "SRV(t0), UAV(u0), RootConstants(num32BitConstants=7, b0)"
ByteAddressBuffer source : register(t0);
RWByteAddressBuffer target : register(u0);
cbuffer Params : register(b0) {
    uint2 srcBase;
    uint2 dstBase;
    uint srcBytes;
    uint dstBytes;
    uint hops;
};
uint2 addOffset(uint2 base, uint delta) {
    uint lo = base.x + delta;
    return uint2(lo, base.y + uint(lo < base.x));
}
bool offsetOf(uint2 address, uint2 base, uint bytes, uint width, out uint at) {
    at = address.x - base.x;
    uint hi = address.y - base.y - uint(address.x < base.x);
    return hi == 0 && (at & 3) == 0 && width <= bytes && at <= bytes - width;
}
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint2 address = addOffset(srcBase, id.x * 16);
    uint value = 0;
    for (uint step = 0; step < hops; ++step) {
        uint at;
        if (!offsetOf(address, srcBase, srcBytes, 16, at)) {
            value = 0;
            break;
        }
        uint4 node = source.Load4(at);
        address = node.xy;
        value ^= node.z;
    }
    uint at;
    // The last lane attempts a deliberately misaligned store.
    uint delta = id.x * 4 + uint(id.x * 4 == dstBytes - 4);
    if (offsetOf(addOffset(dstBase, delta), dstBase, dstBytes, 4, at))
        target.Store(at, value);
}
"#;

fn halves(address: u64) -> [u32; 2] {
    [address as u32, (address >> 32) as u32]
}

fn next(node: usize) -> usize {
    (node + 8191) % NODES
}

fn value(node: usize, seed: u32) -> u32 {
    (node as u32 ^ seed).wrapping_mul(0xa34e_d927)
}

fn expected(mut node: usize, seed: u32, hops: u32) -> u32 {
    if node == NODES - 1 {
        return SENTINEL;
    }
    let mut result = 0;
    for step in 0..hops {
        result ^= value(node, seed);
        if node == 0 && step + 1 < hops {
            return 0;
        }
        node = next(node);
    }
    result
}

#[test]
#[ignore = "requires a selected native D3D12 adapter"]
fn pointer_chasing_through_translated_root_windows() {
    assert!(std::env::var_os("VICTORY_SHARED_TEST_ADAPTER").is_some());
    let host = host().expect("D3D12 device");
    let (root, pso) = pipeline(host, WINDOW_SHADER);
    let mut title = TitleBuffer::new();
    let title_addr = title.0.as_ptr() as u64;
    let shadow = crate::shadow::find(title_addr, BYTES).unwrap();
    let prefix = host.unified_buffer(256).expect("shared arena");
    let shared = host.unified_buffer(BYTES).expect("input window");
    let rest = host
        .unified_buffer(UNIFIED_ARENA_SIZE - shared.offset - BYTES)
        .unwrap();
    assert_eq!(shared.offset, prefix.offset + 256);
    assert_eq!(rest.offset, shared.offset + BYTES);
    assert!(host.unified_buffer(BYTES).is_none());
    assert_ne!(shared.gpu, GUEST_READ);
    assert_ne!(shared.gpu, shared.data as u64);

    const OUT_BYTES: u64 = NODES as u64 * 4;
    let (output, mapped) = host.shared_buffer(OUT_BYTES).unwrap();
    let timestamps = Timestamps::new(&host.device);
    // SAFETY: the direct queue is live.
    let frequency = unsafe { host.queue.GetTimestampFrequency() }.unwrap() as f64;
    let list = host.new_list().unwrap();
    let [src_lo, src_hi] = halves(GUEST_READ);
    let [dst_lo, dst_hi] = halves(GUEST_WRITE);
    let timing = std::env::var_os("VICTORY_SHARED_TEST_TIMING").is_some();
    let pairs = if timing { 6 } else { 2 };
    for hops in [1, 8] {
        let mut rows = [Vec::new(), Vec::new()];
        for sample in 0..2 * pairs {
            let with_shared = sample % 4 == 1 || sample % 4 == 2;
            let seed = (sample / 2) as u32 * 15 + hops;
            let nodes: Vec<u32> = (0..NODES)
                .flat_map(|node| {
                    let ptr = GUEST_READ
                        + if node == 0 {
                            BYTES
                        } else {
                            next(node) as u64 * 16
                        };
                    let [lo, hi] = halves(ptr);
                    [lo, hi, value(node, seed), 0]
                })
                .collect();
            let cpu = Instant::now();
            let gpu = if with_shared {
                // SAFETY: this held suballocation is large enough and has no in-flight reads.
                unsafe {
                    std::ptr::copy_nonoverlapping(nodes.as_ptr(), shared.data.cast(), nodes.len())
                };
                shared.gpu
            } else {
                title.0.copy_from_slice(&nodes);
                shadow.refresh(&list, title_addr, BYTES);
                shadow.gpu(title_addr)
            };
            let fill_us = cpu.elapsed().as_micros();
            let constants = [
                src_lo,
                src_hi,
                dst_lo,
                dst_hi,
                BYTES as u32,
                OUT_BYTES as u32,
                hops,
            ];
            // SAFETY: resources, root signature and PSO stay live through completion.
            // The shader bounds-checks both root views before accessing them.
            unsafe {
                mapped.cast::<u32>().add(NODES - 1).write(SENTINEL);
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list.SetComputeRootShaderResourceView(0, gpu);
                list.list
                    .SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
                list.list.SetComputeRoot32BitConstants(
                    2,
                    constants.len() as u32,
                    constants.as_ptr().cast(),
                    0,
                );
                list.list
                    .EndQuery(&timestamps.0, D3D12_QUERY_TYPE_TIMESTAMP, 0);
                list.list.Dispatch(NODES as u32 / 64, 1, 1);
                list.list
                    .EndQuery(&timestamps.0, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                list.list.ResolveQueryData(
                    &timestamps.0,
                    D3D12_QUERY_TYPE_TIMESTAMP,
                    0,
                    2,
                    &timestamps.1,
                    0,
                );
            }
            list.close();
            let t = Instant::now();
            let prep = SUBMISSION_US[0].load(Ordering::Relaxed);
            host.execute(&[&list]);
            let submit_us = t.elapsed().as_micros();
            let prep_us = SUBMISSION_US[0].load(Ordering::Relaxed) - prep;
            let gpu_us = timestamps.completed_ticks() as f64 / frequency * 1e6;
            if sample >= 2 {
                rows[usize::from(with_shared)].push([
                    fill_us as f64,
                    prep_us as f64,
                    gpu_us,
                    submit_us as f64,
                ]);
            }
            // SAFETY: the synchronous submission completed all UAV writes.
            let got = unsafe { std::slice::from_raw_parts(mapped.cast::<u32>(), NODES) };
            for (word, &got) in got.iter().enumerate() {
                assert_eq!(
                    got,
                    expected(word, seed, hops),
                    "word={word} shared={with_shared} hops={hops}"
                );
            }
            list.reset();
        }
        if timing {
            for (shared, samples) in rows.iter().enumerate() {
                let median = |i: usize| {
                    let mut values: Vec<_> = samples.iter().map(|row| row[i]).collect();
                    values.sort_by(f64::total_cmp);
                    values[values.len() / 2]
                };
                eprintln!(
                    "hops={hops} shared={shared} fill_us={:.2} prep_us={:.2} gpu_us={:.2} submit_us={:.2}",
                    median(0),
                    median(1),
                    median(2),
                    median(3)
                );
            }
        }
    }
    if !timing {
        let queue: ID3D12InfoQueue = host.device.cast().expect("debug info queue");
        // SAFETY: a live debug queue associated with this test device.
        unsafe {
            for index in 0..queue.GetNumStoredMessagesAllowedByRetrievalFilter() {
                let mut len = 0;
                queue.GetMessage(index, None, &mut len).unwrap();
                let mut storage = vec![0usize; len.div_ceil(size_of::<usize>())];
                let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
                queue.GetMessage(index, Some(message), &mut len).unwrap();
                let msg = &*message;
                let text = std::slice::from_raw_parts(msg.pDescription, msg.DescriptionByteLength);
                assert!(
                    msg.Severity.0 >= D3D12_MESSAGE_SEVERITY_INFO.0,
                    "{}",
                    String::from_utf8_lossy(text)
                );
            }
        }
    }
}

struct Timestamps(ID3D12QueryHeap, ID3D12Resource);

impl Timestamps {
    fn new(device: &ID3D12Device) -> Self {
        let mut queries = None;
        let mut readback = None;
        // SAFETY: all descriptions and output storage are live.
        unsafe {
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 2,
                        NodeMask: 0,
                    },
                    &mut queries,
                )
                .unwrap();
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_READBACK,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: 16,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        Self(queries.unwrap(), readback.unwrap())
    }

    fn completed_ticks(&self) -> u64 {
        // SAFETY: the test calls this after waiting for the resolving submission.
        unsafe {
            let mut ptr = std::ptr::null_mut();
            let range = D3D12_RANGE { Begin: 0, End: 16 };
            self.1.Map(0, Some(&range), Some(&mut ptr)).unwrap();
            let pair = ptr.cast::<[u64; 2]>().read();
            self.1.Unmap(0, Some(&D3D12_RANGE::default()));
            pair[1] - pair[0]
        }
    }
}
