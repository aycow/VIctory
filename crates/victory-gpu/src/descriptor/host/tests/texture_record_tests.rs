//! Direct-heap reads after an unsubmitted upload recording.

use super::{RESOURCE_IID, make_shader_heap};
use crate::descriptor::{create_placed_srv, create_srv, heap};
use crate::host::{Host, HostList, install_bindless_test_device};
use crate::object::{S_OK, This, slot};
use crate::resource;
use std::ffi::CString;
use std::mem::ManuallyDrop;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCSTR};

const INPUT_OLD: u32 = 0x1234_5678;
const INPUT_NEW: u32 = 0x8765_4321;
const INPUT_GPU: u32 = 0xfeed_1234;
const BYTES: u64 = 64 << 10;
const READ_STATE: u32 = (D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0
    | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0) as u32;
const HLSL: &str = r#"
#define RS "RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED), UAV(u0)"
RWByteAddressBuffer output : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    Texture2D<uint> source = ResourceDescriptorHeap[0];
    output.Store(0, source.Load(int3(0, 0, 0)));
}
"#;

fn device() -> (ID3D12Device, String) {
    let sdk = std::env::var("VICTORY_BINDLESS_TEST_SDK").expect("Agility 1.620.x x64 DLL path");
    let sdk = CString::new(format!("{}\\", sdk.trim_end_matches(['/', '\\']))).unwrap();
    let adapter = std::env::var("VICTORY_BINDLESS_TEST_ADAPTER").expect("amd, nvidia or warp");
    // SAFETY: SDK path and descriptor output parameters are live.
    unsafe {
        let mut configuration: Option<ID3D12SDKConfiguration1> = None;
        D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut configuration).unwrap();
        let factory: ID3D12DeviceFactory = configuration
            .unwrap()
            .CreateDeviceFactory(620, PCSTR(sdk.as_ptr().cast()))
            .unwrap();
        let debug: ID3D12Debug1 = factory
            .GetConfigurationInterface(&CLSID_D3D12Debug)
            .unwrap();
        debug.EnableDebugLayer();
        debug.SetEnableGPUBasedValidation(true);
        let dxgi: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let selected: IDXGIAdapter1 = match adapter.as_str() {
            "warp" => dxgi.EnumWarpAdapter().unwrap(),
            other => {
                let vendor = match other {
                    "amd" => 0x1002,
                    "nvidia" => 0x10de,
                    _ => panic!("unknown adapter {other}"),
                };
                (0..)
                    .map_while(|i| dxgi.EnumAdapters1(i).ok())
                    .find(|a| a.GetDesc1().unwrap().VendorId == vendor)
                    .expect("requested hardware adapter")
            }
        };
        let desc = selected.GetDesc1().unwrap();
        assert_eq!(
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
            adapter == "warp"
        );
        let name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .into();
        let mut device: Option<ID3D12Device> = None;
        factory
            .CreateDevice(&selected, D3D_FEATURE_LEVEL_12_0, &mut device)
            .unwrap();
        let device = device.unwrap();
        let config = device
            .cast::<ID3D12DeviceConfiguration>()
            .unwrap()
            .GetDesc();
        assert_ne!(
            config.Flags.0 & D3D12_DEVICE_FLAG_GPU_BASED_VALIDATION_ENABLED.0,
            0
        );
        assert_eq!(
            config.GpuBasedValidationFlags
                & D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING.0 as u32,
            0
        );
        (device, name)
    }
}

fn pipeline(host: &Host) -> (ID3D12RootSignature, ID3D12PipelineState) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("sample.hlsl");
    let output = dir.path().join("sample.dxil");
    std::fs::write(&source, HLSL).unwrap();
    let compiler = std::env::var_os("VICTORY_BINDLESS_TEST_DXC").expect("path to DXC");
    let result = std::process::Command::new(compiler)
        .args(["-E", "main", "-T", "cs_6_6", "-Fo"])
        .arg(&output)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let code = std::fs::read(output).unwrap();
    // SAFETY: original DXIL contains a root signature and is live through PSO creation.
    unsafe {
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).unwrap();
        let pso = host
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

fn make_texture(memory: u64, handle: u64, placed: bool) -> (This, Option<resource::HeldTexture>) {
    let words = [
        3,
        0,
        0,
        0,
        4,
        0,
        4,
        0x0001_0001,
        DXGI_FORMAT_R32_UINT.0 as u32,
        1,
        0,
        0x100,
        0,
        0,
    ];
    let null = std::ptr::null();
    if placed {
        let mut srv = [0; 16];
        srv[0] = DXGI_FORMAT_R32_UINT.0 as u32;
        srv[1] = D3D12_SRV_DIMENSION_TEXTURE2D.0 as u32;
        srv[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        srv[5] = 1;
        srv[10] = memory as u32;
        srv[11] = (memory >> 32) as u32;
        create_placed_srv(null, &words, &srv, handle);
        let held = resource::texture_at(memory, &words).unwrap();
        (held.as_this(), Some(held))
    } else {
        let mut result = null;
        assert_eq!(
            resource::create_at_address(
                null,
                memory,
                words.as_ptr().cast(),
                0,
                std::ptr::null(),
                &RESOURCE_IID,
                &mut result,
            ),
            S_OK
        );
        create_srv(null, result, std::ptr::null(), handle);
        (result, None)
    }
}

fn check_debug(info: &ID3D12InfoQueue) {
    let mut issues = Vec::new();
    // SAFETY: the message buffers use byte counts and alignment required by D3D12.
    unsafe {
        for i in 0..info.GetNumStoredMessages() {
            let mut size = 0;
            info.GetMessage(i, None, &mut size).unwrap();
            let mut words = vec![0u64; size.div_ceil(8)];
            let ptr = words.as_mut_ptr().cast::<D3D12_MESSAGE>();
            info.GetMessage(i, Some(ptr), &mut size).unwrap();
            if (*ptr).Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
                issues.push(
                    std::ffi::CStr::from_ptr((*ptr).pDescription.cast())
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    assert!(issues.is_empty(), "{issues:?}");
}

fn sample(
    consumer: &HostList,
    heap: &ID3D12DescriptorHeap,
    root: &ID3D12RootSignature,
    pipeline: &ID3D12PipelineState,
    output: &ID3D12Resource,
) {
    // SAFETY: the output, pipeline and view heap outlive the dispatch.
    unsafe {
        consumer.list.SetDescriptorHeaps(&[Some(heap.clone())]);
        consumer.list.SetComputeRootSignature(root);
        consumer.list.SetPipelineState(pipeline);
        consumer
            .list
            .SetComputeRootUnorderedAccessView(0, output.GetGPUVirtualAddress());
        consumer.list.Dispatch(1, 1, 1);
    }
}

#[test]
#[ignore = "requires DXC, Agility SDK 1.620.x and a selected adapter"]
fn unsubmitted_first_recording_does_not_consume_a_bindless_texture_upload() {
    let (device, name) = device();
    let info = device.cast::<ID3D12InfoQueue>().unwrap();
    let host = install_bindless_test_device(device);
    let (root, pipeline) = pipeline(host);
    let (output, mapped) = host.shared_buffer(BYTES).unwrap();
    let explicit_retry = std::env::var_os("VICTORY_BINDLESS_TEST_REARM").is_some();
    let mut failures = Vec::new();
    for (placed, discard) in [(false, true), (true, true), (false, false), (true, false)] {
        // Use a separate heap per case so the record-time cache cannot hide
        // the first bindless use. Seed the destination through the uploader
        // independently to give the stale case deterministic valid data.
        let (title_heap, handle) = make_shader_heap(8);
        let native_heap = heap(title_heap).host_arc().unwrap();
        let memory = resource::Memory::zeroed(BYTES, BYTES).unwrap();
        let address = memory.address();
        // SAFETY: staging is synchronous in this branch, and the allocation holds the pixel.
        unsafe { (address as *mut u32).write(INPUT_OLD) };
        let (texture, _held_texture) = make_texture(address, handle, placed);
        let native = resource::host_resource(texture).unwrap();
        let seed = host.new_list().unwrap();
        crate::texture::upload(&seed, texture, native);
        crate::list::ensure_state(&seed, texture, READ_STATE);
        seed.close();
        host.execute(&[&seed]);
        // SAFETY: no GPU access is pending and CPU memory is still held.
        unsafe { (address as *mut u32).write(INPUT_NEW) };
        let first = host.new_list().unwrap();
        native_heap.prepare_bindless_record(&first);
        first.defer_bindless(&native_heap, false);
        first.close();
        let first = (!discard).then_some(first);

        let consumer = host.new_list().unwrap();
        native_heap.prepare_bindless_record(&consumer);
        consumer.defer_bindless(&native_heap, false);
        if explicit_retry {
            // Reference for the scheduler: the consuming list holds its own
            // upload request and #230 decides the native copy at submission.
            crate::texture::upload(&consumer, texture, native);
            crate::list::ensure_state(&consumer, texture, READ_STATE);
        }
        // SAFETY: this output mapping is idle until submission.
        unsafe { mapped.cast::<u32>().write(0) };
        sample(&consumer, &native_heap.heap, &root, &pipeline, &output);
        consumer.close();
        host.execute(&[&consumer]);
        // SAFETY: submission is fenced before reading the output mapping.
        let got = unsafe { mapped.cast::<u32>().read() };
        eprintln!(
            "{name} placed={placed} discard={discard}: expected={INPUT_NEW:#x} observed={got:#x}"
        );
        if got != INPUT_NEW {
            failures.push((placed, discard, got));
        }
        if let Some(first) = first {
            host.execute(&[&first]);
        }
        if !placed {
            slot::<extern "system" fn(This) -> u32>(texture, 0x10)(texture);
        }
        slot::<extern "system" fn(This) -> u32>(title_heap, 0x10)(title_heap);
    }
    check_debug(&info);
    assert!(
        failures.is_empty(),
        "first bindless use read the previous texture contents: {failures:?}"
    );
}

#[test]
#[ignore = "requires DXC, Agility SDK 1.620.x and a selected adapter"]
fn a_bound_bindless_upload_preserves_an_earlier_submitted_gpu_producer() {
    let (device, name) = device();
    let info = device.cast::<ID3D12InfoQueue>().unwrap();
    let host = install_bindless_test_device(device);
    let (root, pipeline) = pipeline(host);
    let (output, mapped) = host.shared_buffer(BYTES).unwrap();
    // Keep distinct addresses live in every case. A freed address can be
    // reused while the placed cache still names its prior texture.
    let backings: [_; 4] = std::array::from_fn(|_| {
        (
            resource::Memory::zeroed(BYTES, BYTES).unwrap(),
            resource::Memory::zeroed(BYTES, BYTES).unwrap(),
        )
    });
    for placed in [false, true] {
        for early in [false, true] {
            let (title_heap, handle) = make_shader_heap(8);
            let native_heap = heap(title_heap).host_arc().unwrap();
            let (cpu, producer_memory) = &backings[2 * usize::from(placed) + usize::from(early)];
            // SAFETY: each allocation holds a full texture, still idle.
            unsafe {
                (cpu.address() as *mut u32).write(INPUT_NEW);
                (producer_memory.address() as *mut u32).write(INPUT_GPU);
            }
            let (target, _target_placed) = make_texture(cpu.address(), handle, placed);
            let target_host = resource::host_resource(target).unwrap();
            let record = || {
                let list = host.new_list().unwrap();
                native_heap.prepare_bindless_record(&list);
                list.defer_bindless(&native_heap, false);
                sample(&list, &native_heap.heap, &root, &pipeline, &output);
                list.close();
                list
            };
            let early_consumer = early.then(record);

            let (source, _source_placed) =
                make_texture(producer_memory.address(), handle + 32, false);
            let source_host = resource::host_resource(source).unwrap();
            let producer = host.new_list().unwrap();
            crate::texture::upload(&producer, source, source_host);
            crate::list::ensure_state(&producer, source, D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32);
            crate::list::ensure_state(&producer, target, D3D12_RESOURCE_STATE_COPY_DEST.0 as u32);
            // SAFETY: equal-format, distinct textures in native copy states.
            unsafe { producer.list.CopyResource(target_host, source_host) };
            crate::texture::defer_host_write(&producer, target);
            crate::list::ensure_state(&producer, target, READ_STATE);
            producer.close();
            host.execute(&[&producer]);

            let consumer = early_consumer.unwrap_or_else(record);
            host.execute(&[&consumer]);
            // SAFETY: the synchronous submission completed the single result word.
            let observed = unsafe { mapped.cast::<u32>().read() };
            eprintln!(
                "{name} placed={placed} upload_recorded_before_producer={early}: {observed:#x}"
            );
            assert_eq!(observed, INPUT_GPU);
            for resource in [source, title_heap] {
                slot::<extern "system" fn(This) -> u32>(resource, 0x10)(resource);
            }
            if !placed {
                slot::<extern "system" fn(This) -> u32>(target, 0x10)(target);
            }
        }
    }
    check_debug(&info);
}
