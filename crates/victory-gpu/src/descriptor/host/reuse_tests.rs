//! Original 65,536-slot D3D12 allocator and descriptor readback fixture.
//! Set VICTORY_TABLE_TEST_ADAPTER (amd, nvidia or warp), then run:
//! cargo test --release -p victory-gpu --features diagnostics capped_pool_texture_table_reuse -- --ignored --nocapture
//! Set VICTORY_TABLE_TEST_DEBUG=1 for the system D3D12 debug layer or
//! VICTORY_TABLE_TEST_GBV=1 for GPU-based validation, where supported.
//! VICTORY_TABLE_TEST_TIMING=1 runs five alternating timing samples.

use super::*;
use std::mem::ManuallyDrop;
use std::time::Instant;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, s};

const HLSL: &str = r#"
#define RS "RootConstants(num32BitConstants=1, b0), UAV(u0), DescriptorTable(SRV(t0))"
cbuffer Constants : register(b0) { uint index; };
RWByteAddressBuffer output : register(u0);
Texture2D<uint> image : register(t0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    output.Store(index * 4, image.Load(int3(0, 0, 0)));
}
"#;
const FIRST: u32 = 0x1458_acef;
const SECOND: u32 = 0xfec1_4921;
const SPAN: u32 = 2048;

fn device(debug: bool, gbv: bool) -> (ID3D12Device, String) {
    let vendor = std::env::var("VICTORY_TABLE_TEST_ADAPTER").expect("amd, nvidia or warp");
    // SAFETY: the debug interface, adapter enumeration and output pointers are ours.
    unsafe {
        if debug {
            let mut debug: Option<ID3D12Debug> = None;
            D3D12GetDebugInterface(&mut debug).expect("install Windows Graphics Tools for the D3D12 debug layer");
            let debug = debug.unwrap();
            debug.EnableDebugLayer();
            if gbv {
                debug
                    .cast::<ID3D12Debug1>()
                    .unwrap()
                    .SetEnableGPUBasedValidation(true);
            }
        }
        let dxgi: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = if vendor == "warp" {
            dxgi.EnumWarpAdapter().unwrap()
        } else {
            let vendor = match vendor.as_str() {
                "amd" => 0x1002,
                "nvidia" => 0x10de,
                _ => panic!("unknown adapter {vendor}"),
            };
            (0..)
                .map_while(|i| dxgi.EnumAdapters1(i).ok())
                .find(|a| a.GetDesc1().unwrap().VendorId == vendor)
                .expect("requested hardware adapter")
        };
        let desc = adapter.GetDesc1().unwrap();
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..end]);
        assert_eq!(
            vendor == "warp",
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
        );
        let mut device = None;
        D3D12CreateDevice(&adapter.cast::<IDXGIAdapter>().unwrap(), D3D_FEATURE_LEVEL_11_0, &mut device).unwrap();
        (device.unwrap(), name)
    }
}

fn shader(hlsl: &str) -> Vec<u8> {
    let mut code = None;
    // SAFETY: the source and names live for the call, and the output blob is ours.
    unsafe {
        D3DCompile(
            hlsl.as_ptr().cast(),
            hlsl.len(),
            s!("descriptor reuse fixture"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            None,
        )
        .expect("compile descriptor reuse fixture");
        let code = code.unwrap();
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize()).to_vec()
    }
}

fn compute_pipeline(
    device: &ID3D12Device,
    code: &[u8],
) -> (ID3D12RootSignature, ID3D12PipelineState) {
    // SAFETY: live device and compiled compute shader with an embedded root signature.
    unsafe {
        let root = device.CreateRootSignature(0, code).unwrap();
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(ID3D12RootSignature::clone(&root))),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            },
            ..Default::default()
        };
        let pipeline = device.CreateComputePipelineState(&desc);
        drop(ManuallyDrop::into_inner(desc.pRootSignature));
        (root, pipeline.unwrap())
    }
}

fn resource(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
    desc: &D3D12_RESOURCE_DESC,
) -> ID3D12Resource {
    let mut resource = None;
    // SAFETY: caller supplies a valid resource description and initial state.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: heap,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                desc,
                state,
                None,
                &mut resource,
            )
            .unwrap();
    }
    resource.unwrap()
}

fn buf(width: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: width,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

fn list(device: &ID3D12Device) -> (ID3D12CommandAllocator, ID3D12GraphicsCommandList) {
    // SAFETY: creates a direct list and its matching allocator.
    unsafe {
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        (allocator, list)
    }
}

struct Fixture {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    textures: [ID3D12Resource; 2],
    upload: ID3D12Resource,
}

impl Fixture {
    fn new(device: ID3D12Device) -> Self {
        let code = shader(HLSL);
        let (root, pso) = compute_pipeline(&device, &code);
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Format: DXGI_FORMAT_R32_UINT,
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            ..buf(1, D3D12_RESOURCE_FLAG_NONE)
        };
        let textures = [(); 2].map(|()| {
            resource(
                &device,
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_RESOURCE_STATE_COPY_DEST,
                &desc,
            )
        });
        let upload = resource(
            &device,
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            &buf(1024, D3D12_RESOURCE_FLAG_NONE),
        );
        // SAFETY: valid pipeline inputs and live resources. The upload is
        // initialized before the command list is submitted, then kept alive.
        unsafe {
            let queue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default())
                .unwrap();
            let mut data = std::ptr::null_mut();
            upload
                .Map(0, Some(&D3D12_RANGE::default()), Some(&mut data))
                .unwrap();
            data.cast::<u8>().write_bytes(0, 1024);
            data.cast::<u32>().write(FIRST);
            data.cast::<u32>().add(128).write(SECOND);
            upload.Unmap(0, None);
            let (_allocator, setup) = list(&device);
            for (i, texture) in textures.iter().enumerate() {
                let src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(upload.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: (i * 512) as u64,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: DXGI_FORMAT_R32_UINT,
                                Width: 1,
                                Height: 1,
                                Depth: 1,
                                RowPitch: 256,
                            },
                        },
                    },
                };
                let dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(texture.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: 0,
                    },
                };
                setup.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                drop(ManuallyDrop::into_inner(src.pResource));
                drop(ManuallyDrop::into_inner(dst.pResource));
                setup.ResourceBarrier(&[crate::host::transition(
                    texture,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                )]);
            }
            setup.Close().unwrap();
            let fixture = Self {
                device,
                queue,
                root,
                pso,
                textures,
                upload,
            };
            fixture.submit(&[setup]);
            fixture
        }
    }

    fn submit(&self, lists: &[ID3D12GraphicsCommandList]) {
        let lists: Vec<_> = lists
            .iter()
            .map(|list| Some(list.cast::<ID3D12CommandList>().unwrap()))
            .collect();
        // SAFETY: closed lists and their live allocators, fence owned by this test.
        unsafe {
            self.queue.ExecuteCommandLists(&lists);
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            self.queue.Signal(&fence, 1).unwrap();
            fence.SetEventOnCompletion(1, Default::default()).unwrap();
            self.device.GetDeviceRemovedReason().unwrap();
        }
    }
}

fn capped_heap(device: &ID3D12Device) -> HostHeap {
    let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
    let total = 1 + MIN_DYNAMIC;
    // SAFETY: a plain shader-visible heap on the fixture's own device.
    let (heap, cpu, step) = unsafe {
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: total,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
            .unwrap();
        let cpu = heap.GetCPUDescriptorHandleForHeapStart().ptr;
        let step = device.GetDescriptorHandleIncrementSize(kind);
        (heap, cpu, step)
    };
    HostHeap {
        heap,
        cpu,
        step,
        count: 1,
        dynamic: MIN_DYNAMIC,
        sampler: false,
        marks: None,
        made: Mutex::new(vec![0]),
        kept: Mutex::new(vec![None; total as usize]),
        slots: Arc::new(DynamicSlots::new(1, MIN_DYNAMIC)),
        pairs: Mutex::new((total, Vec::new())),
        pairs_end: total,
        strides: None,
        addresses: Default::default(),
        viewed: Default::default(),
    }
}

fn descriptor() -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    }
}

fn occupancy(heap: &HostHeap) -> usize {
    heap.slots
        .next
        .lock()
        .unwrap()
        .1
        .iter()
        .filter(|&&used| used)
        .count()
}

fn heap_replacement(fixture: &Fixture) {
    let view = View::Shader(0, None);
    let views = [TextureView {
        slot: 0,
        record: 1 << 32,
        view: &view,
        resource: fixture.textures[0].clone(),
        desc: descriptor(),
    }];
    let original = capped_heap(&fixture.device);
    let original_pool = Arc::downgrade(&original.slots);
    let (original_table, hit) = original
        .shared_textures_on(&fixture.device, SPAN, &views)
        .unwrap();
    assert!(!hit);
    let list_local = original.reserve_dynamic(SPAN).unwrap();
    let original_id = original.identity();
    drop(original);

    let replacement = capped_heap(&fixture.device);
    let (replacement_table, hit) = replacement
        .shared_textures_on(&fixture.device, SPAN, &views)
        .unwrap();
    assert!(!hit);
    assert_ne!(original_id, replacement.identity());
    assert_eq!(original_table.first(), replacement_table.first());
    assert!(!Arc::ptr_eq(&original_table, &replacement_table));
    assert_ne!(
        original_table._heap.as_raw(),
        replacement_table._heap.as_raw()
    );
    drop(original_table);
    assert!(original_pool.upgrade().is_some());
    drop(list_local);
    assert!(original_pool.upgrade().is_none());
    assert_eq!(occupancy(&replacement), SPAN as usize);
}

struct ResultRow {
    failed: usize,
    occupied: usize,
    preparing: std::time::Duration,
}

fn run(fixture: &Fixture, shared: bool, lists: usize, distinct: bool) -> ResultRow {
    let device = &fixture.device;
    let heap = capped_heap(device);
    let width = (lists * 4) as u64;
    let output = resource(
        device,
        D3D12_HEAP_TYPE_DEFAULT,
        D3D12_RESOURCE_STATE_COMMON,
        &buf(width, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
    );
    let back = resource(
        device,
        D3D12_HEAP_TYPE_READBACK,
        D3D12_RESOURCE_STATE_COPY_DEST,
        &buf(width, D3D12_RESOURCE_FLAG_NONE),
    );
    let (init_allocator, init) = list(device);
    let (end_allocator, end) = list(device);
    let recorded: Vec<_> = (0..lists).map(|_| list(device)).collect();
    let mut failures = 0;
    let mut preparing = std::time::Duration::ZERO;
    let mut work = vec![init.clone()];
    let mut leases = Vec::with_capacity(lists);
    let mut expected = vec![0; lists];

    // SAFETY: all lists are open and all resources remain live through the fence.
    unsafe {
        init.ResourceBarrier(&[crate::host::transition(
            &output,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )]);
        init.CopyBufferRegion(&output, 0, &fixture.upload, 768, width);
        init.ResourceBarrier(&[crate::host::transition(
            &output,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
        init.Close().unwrap();
        for (index, (_, list)) in recorded.iter().enumerate() {
            let rewritten = index + 1 == lists;
            let texture = &fixture.textures[usize::from(rewritten)];
            let view = View::Shader(0, None);
            // New generations can coexist at the same title view index.
            let generation = u64::from(rewritten) + 1;
            let position = if distinct { index as u32 } else { 0 };
            let views = [TextureView {
                slot: position,
                record: generation << 32,
                view: &view,
                resource: texture.clone(),
                desc: descriptor(),
            }];
            let started = Instant::now();
            let slot = if shared {
                heap.shared_textures_on(device, SPAN, &views).map(|(lease, hit)| {
                    assert_eq!(hit, leases.iter().any(|prior| Arc::ptr_eq(prior, &lease)));
                    let first = lease.first();
                    leases.push(lease);
                    first
                })
            } else if let Some(first) = heap.dynamic(SPAN) {
                for slot in first..first + SPAN {
                    if slot == first + position {
                        device.CreateShaderResourceView(
                            texture,
                            Some(&views[0].desc),
                            heap.cpu(slot),
                        );
                        heap.keep(slot, Some((texture.clone(), None)));
                    } else {
                        null_view_on(device, heap.cpu(slot));
                    }
                }
                Some(first)
            } else {
                None
            };
            preparing += started.elapsed();
            if let Some(slot) = slot {
                list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
                list.SetComputeRootSignature(&fixture.root);
                list.SetPipelineState(&fixture.pso);
                list.SetComputeRoot32BitConstant(0, index as u32, 0);
                list.SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
                list.SetComputeRootDescriptorTable(2, heap.gpu(slot + position));
                list.Dispatch(1, 1, 1);
                expected[index] = if rewritten { SECOND } else { FIRST };
                work.push(list.clone());
            } else {
                failures += 1;
            }
            list.Close().unwrap();
        }
        end.ResourceBarrier(&[crate::host::transition(
            &output,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        end.CopyBufferRegion(&back, 0, &output, 0, width);
        end.ResourceBarrier(&[crate::host::transition(
            &output,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COMMON,
        )]);
        end.Close().unwrap();
        work.push(end);
        for _ in 0..2 {
            fixture.submit(&work);
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data)).unwrap();
            assert_eq!(
                std::slice::from_raw_parts(data.cast::<u32>(), lists),
                expected
            );
            back.Unmap(0, Some(&D3D12_RANGE::default()));
        }
        drop((init_allocator, end_allocator, recorded));
    }
    let occupied = occupancy(&heap);
    #[cfg(feature = "diagnostics")]
    if crate::descriptor::demand::enabled() {
        assert_eq!(heap.demand_pressure(), (MIN_DYNAMIC, occupied, occupied));
    }
    if shared {
        let keep = leases.pop().unwrap();
        drop(leases);
        assert_eq!(occupancy(&heap), SPAN as usize);
        drop(keep);
        assert_eq!(occupancy(&heap), 0);
        #[cfg(feature = "diagnostics")]
        if crate::descriptor::demand::enabled() {
            assert_eq!(heap.demand_pressure(), (MIN_DYNAMIC, 0, occupied));
        }
        assert!(heap.slots.shared.lock().unwrap().is_empty());
    }
    ResultRow {
        failed: failures,
        occupied,
        preparing,
    }
}

#[test]
#[ignore = "requires a named D3D12 adapter"]
fn capped_pool_texture_table_reuse() {
    let timing = std::env::var_os("VICTORY_TABLE_TEST_TIMING").is_some();
    let gbv = !timing && std::env::var_os("VICTORY_TABLE_TEST_GBV").is_some();
    let debug = gbv || !timing && std::env::var_os("VICTORY_TABLE_TEST_DEBUG").is_some();
    let (device, name) = device(debug, gbv);
    let info = debug.then(|| device.cast::<ID3D12InfoQueue>().unwrap());
    let fixture = Fixture::new(device);
    for (label, count, distinct) in [
        ("over-cap", 41, false),
        ("equal-work", 24, false),
        ("unique", 24, true),
    ] {
        if timing {
            run(&fixture, false, count, distinct);
            run(&fixture, true, count, distinct);
        }
        let mut samples = [Vec::new(), Vec::new()];
        for sample in 0..if timing { 5 } else { 1 } {
            for shared in [sample % 2 != 0, sample % 2 == 0] {
                let row = run(&fixture, shared, count, distinct);
                let expected_failures = if shared {
                    0
                } else {
                    count.saturating_sub((MIN_DYNAMIC / SPAN) as usize)
                };
                let ranges = if shared && !distinct {
                    2
                } else {
                    count - expected_failures
                };
                assert_eq!(row.failed, expected_failures);
                assert_eq!(row.occupied, ranges * SPAN as usize);
                eprintln!(
                    "{name} {label} shared={shared} failed={} peak={} prepare_ms={:.3}",
                    row.failed,
                    row.occupied,
                    row.preparing.as_secs_f64() * 1000.0
                );
                samples[usize::from(shared)].push(row.preparing);
            }
        }
        for (shared, samples) in samples.iter_mut().enumerate() {
            samples.sort_unstable();
            eprintln!(
                "{name} {label} shared={} median_ms={:.3}",
                shared != 0,
                samples[samples.len() / 2].as_secs_f64() * 1000.0
            );
        }
    }
    if !timing {
        heap_replacement(&fixture);
    }
    if let Some(info) = info {
        check_debug_messages(&info);
    }
}

fn check_debug_messages(info: &ID3D12InfoQueue) {
    // SAFETY: fetch each validation message into its aligned buffer.
    unsafe {
        for i in 0..info.GetNumStoredMessages() {
            let mut bytes = 0;
            info.GetMessage(i, None, &mut bytes).unwrap();
            let mut storage = vec![0u64; bytes.div_ceil(8)];
            let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
            info.GetMessage(i, Some(message), &mut bytes).unwrap();
            let message = &*message;
            assert!(
                message.Severity.0 > D3D12_MESSAGE_SEVERITY_ERROR.0,
                "{}",
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    message.pDescription,
                    message.DescriptionByteLength,
                ))
            );
        }
    }
}
