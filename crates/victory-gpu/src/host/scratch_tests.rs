use super::*;
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D::{Fxc::D3DCompile, ID3DBlob};
use windows::core::s;

pub(super) fn test_device() -> windows::core::Result<Option<ID3D12Device>> {
    let Ok(kind) = std::env::var("VICTORY_SHARED_TEST_ADAPTER") else {
        return Ok(None);
    };
    let sdk = std::env::var("VICTORY_SHARED_TEST_SDK").expect("Agility SDK 1.620.x x64 path");
    let sdk = std::ffi::CString::new(format!("{}\\", sdk.trim_end_matches(['/', '\\']))).unwrap();
    // SAFETY: plain DXGI/Agility queries with live descriptions and output pointers.
    unsafe {
        let mut configuration: Option<ID3D12SDKConfiguration1> = None;
        D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut configuration)?;
        let factory: ID3D12DeviceFactory = configuration
            .unwrap()
            .CreateDeviceFactory(620, windows::core::PCSTR(sdk.as_ptr().cast()))?;
        let validate = std::env::var_os("VICTORY_SHARED_TEST_TIMING").is_none();
        if validate {
            let debug: ID3D12Debug1 = factory.GetConfigurationInterface(&CLSID_D3D12Debug)?;
            debug.EnableDebugLayer();
            debug.SetEnableGPUBasedValidation(true);
        }
        let dxgi: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter1 = if kind == "warp" {
            dxgi.EnumWarpAdapter()?
        } else {
            let vendor = match kind.as_str() {
                "amd" => 0x1002,
                "nvidia" => 0x10de,
                _ => panic!("unknown adapter {kind}"),
            };
            (0..)
                .map_while(|i| dxgi.EnumAdapters1(i).ok())
                .find(|adapter| adapter.GetDesc1().unwrap().VendorId == vendor)
                .expect("requested adapter")
        };
        let desc = adapter.GetDesc1()?;
        assert_eq!(
            kind == "warp",
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
        );
        eprintln!(
            "{} validation={validate}",
            String::from_utf16_lossy(&desc.Description).trim_end_matches('\0')
        );
        let mut device = None;
        factory.CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
        if validate {
            let dev: &ID3D12Device = device.as_ref().unwrap();
            let config = dev.cast::<ID3D12DeviceConfiguration>()?.GetDesc();
            assert_ne!(
                config.Flags.0 & D3D12_DEVICE_FLAG_GPU_BASED_VALIDATION_ENABLED.0,
                0
            );
            assert_eq!(
                config.GpuBasedValidationFlags
                    & D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING.0 as u32,
                0
            );
        }
        Ok(device)
    }
}

const WORDS: usize = 1 << 18;
const BYTES: u64 = (WORDS * size_of::<u32>()) as u64;
const MASK: u32 = 0x9ab4_53ec;
const SOURCE: &str = r#"
#define RS "SRV(t0), UAV(u0), RootConstants(num32BitConstants=2, b0)"
ByteAddressBuffer input : register(t0);
RWByteAddressBuffer output : register(u0);
cbuffer Parameters : register(b0) { uint stride; uint limit; };
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint index = (id.x * stride) & limit;
    output.Store(id.x * 4, input.Load(index * 4) ^ 0x9ab453ec);
}
"#;

mod window_tests;

fn pipeline(host: &Host, source: &str) -> (ID3D12RootSignature, ID3D12PipelineState) {
    let mut blob: Option<ID3DBlob> = None;
    // SAFETY: compiler inputs and outputs are live; the root signature and
    // shader bytecode are borrowed through PSO creation.
    unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("shared-scratch-read"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut blob,
            None,
        )
        .unwrap();
        let blob = blob.unwrap();
        let code = std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize());
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, code).unwrap();
        let pso: ID3D12PipelineState = host
            .device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: ManuallyDrop::new(Some(std::mem::transmute_copy(&root))),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })
            .unwrap();
        (root, pso)
    }
}

struct TitleBuffer(Vec<u32>);

impl TitleBuffer {
    fn new() -> Self {
        let words = vec![0; WORDS];
        let address = words.as_ptr() as usize;
        crate::shadow::register(address, address as u64, BYTES);
        Self(words)
    }
}

impl Drop for TitleBuffer {
    fn drop(&mut self) {
        let address = self.0.as_ptr() as usize;
        crate::shadow::unregister(address, address as u64);
    }
}

#[test]
#[ignore = "requires a D3D12 device and runs an original full-buffer workload"]
fn shared_scratch_matches_shadow() {
    assert!(use_scratch_arena(), "set VICTORY_SHARED_SCRATCH=1");
    let host = host().expect("D3D12 device");
    let validation = host.device.cast::<ID3D12InfoQueue>().ok();
    let mut title = TitleBuffer::new();
    let title_data = title.0.as_mut_ptr().cast::<u8>();
    let title_address = title_data as usize;
    let shadow = crate::shadow::find(title_address as u64, BYTES).unwrap();
    let (output, output_data) = host.shared_buffer(BYTES).unwrap();
    let (root, pso) = pipeline(host, SOURCE);
    let direct_list = host.new_list().unwrap();
    let baseline_list = host.new_list().unwrap();
    assert!(host.unified_buffer(UNIFIED_ALIGNMENT).is_some());
    let (shared_gpu, shared_data) = direct_list.scratch(BYTES).unwrap();
    let arena = host.unified_arena().expect("scratch arena supported");
    assert!(*host.scratch_arena_active.get().unwrap());
    assert_eq!(
        shared_gpu - arena.gpu,
        shared_data as u64 - arena.data as u64
    );
    assert_eq!(shared_gpu - arena.gpu, UNIFIED_ALIGNMENT);
    assert_ne!(shared_gpu, shared_data as u64);
    assert_eq!(shared_gpu % UNIFIED_ALIGNMENT, 0);
    let fallback_capacity = UNIFIED_ARENA_SIZE - SCRATCH_CHUNK - UNIFIED_ALIGNMENT;
    assert!(host.unified_buffer(fallback_capacity).is_some());
    let (fallback, fallback_data, fallback_gpu) = host.scratch_chunk(true).unwrap();
    // SAFETY: both resources were created by this test's host and are live.
    unsafe { assert_eq!(fallback.GetDesc().Width, SCRATCH_CHUNK) };
    direct_list.close();
    direct_list.reset();

    let run =
        |list: &HostList, address: u64, cpu: *mut u8, direct: bool, stride: u32, seed: u32| {
            let started = Instant::now();
            let values = |i: usize| (i as u32 ^ seed).wrapping_mul(0x91de_2f71);
            for i in 0..WORDS {
                // SAFETY: each path has a live, writable CPU mapping of BYTES.
                unsafe { cpu.cast::<u32>().add(i).write(values(i)) };
            }
            let fill = started.elapsed();
            if !direct {
                shadow.refresh(list, title_address as u64, BYTES);
            }
            // SAFETY: this list and its shader bindings stay live until completion.
            unsafe {
                let constants = [stride, WORDS as u32 - 1];
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list.SetComputeRootShaderResourceView(0, address);
                list.list
                    .SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
                list.list
                    .SetComputeRoot32BitConstants(2, 2, constants.as_ptr().cast(), 0);
                list.list.Dispatch(WORDS as u32 / 64, 1, 1);
            }
            list.close();
            let before = SUBMISSION_US.each_ref().map(|t| t.load(Ordering::Relaxed));
            host.execute(&[list]);
            let after = SUBMISSION_US.each_ref().map(|t| t.load(Ordering::Relaxed));
            // SAFETY: the synchronous host fence completed the output writes.
            let result = unsafe { std::slice::from_raw_parts(output_data.cast::<u32>(), WORDS) };
            for (i, &got) in result.iter().enumerate() {
                assert_eq!(
                    got,
                    values((i * stride as usize) & (WORDS - 1)) ^ MASK,
                    "word {i}"
                );
            }
            list.reset();
            (
                fill,
                Duration::from_micros(after[0] - before[0]),
                Duration::from_micros(after[1] - before[1]),
            )
        };
    for stride in [1, 17, 257] {
        let mut times = [Vec::new(), Vec::new()];
        for sample in 0..6 {
            for shared in [sample % 2 == 0, sample % 2 != 0] {
                let measured = if shared {
                    assert_eq!(direct_list.scratch(BYTES).unwrap().0, shared_gpu);
                    run(&direct_list, shared_gpu, shared_data, true, stride, sample)
                } else {
                    run(
                        &baseline_list,
                        shadow.gpu(title_address as u64),
                        title_data,
                        false,
                        stride,
                        sample,
                    )
                };
                if sample > 0 {
                    times[usize::from(shared)].push(measured);
                }
            }
        }
        for (shared, samples) in times.into_iter().enumerate() {
            let median = |metric: fn((Duration, Duration, Duration)) -> Duration| {
                let mut values = samples.iter().copied().map(metric).collect::<Vec<_>>();
                values.sort_unstable();
                values[2].as_micros()
            };
            eprintln!(
                "stride={stride} shared={shared} fill_us={} prepare_us={} queue_fence_us={}",
                median(|sample| sample.0),
                median(|sample| sample.1),
                median(|sample| sample.2)
            );
        }
    }
    run(
        &direct_list,
        fallback_gpu,
        fallback_data as *mut u8,
        true,
        17,
        9,
    );
    if let Some(queue) = validation {
        // SAFETY: GetMessage is given suitably aligned, queried storage.
        unsafe {
            for index in 0..queue.GetNumStoredMessages() {
                let mut len = 0;
                queue.GetMessage(index, None, &mut len).unwrap();
                let mut storage = vec![0usize; len.div_ceil(size_of::<usize>())];
                let msg = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
                queue.GetMessage(index, Some(msg), &mut len).unwrap();
                let msg = &*msg;
                let description =
                    std::slice::from_raw_parts(msg.pDescription, msg.DescriptionByteLength);
                assert!(
                    msg.Severity.0 > D3D12_MESSAGE_SEVERITY_ERROR.0,
                    "{}",
                    String::from_utf8_lossy(description)
                );
            }
        }
    }
}
