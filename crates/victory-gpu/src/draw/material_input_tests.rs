//! Synthetic constants fed by an earlier native GPU producer.

use super::root_view;
use crate::descriptor::host::HostHeap;
use crate::host::{Host, HostList, host, transition};
use crate::shadow;
use windows::Win32::Graphics::Direct3D::{Fxc::D3DCompile, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{Interface, PCSTR, s};

const SOURCE: &str = r#"
cbuffer Material : register(b0) { uint input_value; }
RWByteAddressBuffer result : register(u0);

[RootSignature("UAV(u0)")]
[numthreads(1,1,1)] void producer() { result.Store(0, 29); }

[RootSignature("CBV(b0), UAV(u0)")]
[numthreads(1,1,1)] void direct_reader() { result.Store(0, input_value); }

[RootSignature("DescriptorTable(CBV(b0)), UAV(u0)")]
[numthreads(1,1,1)] void table_reader() { result.Store(0, input_value); }
"#;

struct Shader {
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
}

impl Shader {
    fn new(host: &Host, entry: PCSTR) -> Self {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // SAFETY: the source, entry name and output pointers stay live.
        unsafe {
            let result = D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("synthetic material input"),
                None,
                None,
                entry,
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            );
            if let Err(error) = result {
                let diagnostics = errors.as_ref().map(|blob| {
                    String::from_utf8_lossy(std::slice::from_raw_parts(
                        blob.GetBufferPointer().cast::<u8>(),
                        blob.GetBufferSize(),
                    ))
                });
                panic!("compilation failed: {error}, {diagnostics:?}");
            }
            let code = code.unwrap();
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root = host.device.CreateRootSignature(0, bytes).unwrap();
            let pipeline = host
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
            Self { root, pipeline }
        }
    }

    fn bind(&self, list: &HostList) {
        // SAFETY: recording to an open command list with live fixture objects.
        unsafe {
            list.list.SetComputeRootSignature(&self.root);
            list.list.SetPipelineState(&self.pipeline);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Input {
    AlignedRoot,
    StagedRoot,
    StagedTable,
}

#[derive(Clone, Copy, Debug)]
enum Order {
    CpuAfterRecording,
    GpuSameSubmission,
    GpuSeparateSubmissions,
}

struct TitleInput(Box<[u32; 128]>);

impl TitleInput {
    fn new() -> Self {
        let words = Box::new([11; 128]);
        let base = words.as_ptr() as usize;
        shadow::register(base, base as u64, size_of_val(&*words) as u64);
        Self(words)
    }
}

impl Drop for TitleInput {
    fn drop(&mut self) {
        let base = self.0.as_ptr() as usize;
        shadow::unregister(base, base as u64);
    }
}

fn run(host: &Host, shaders: &[Shader; 3], input: Input, order: Order) -> (u32, u32) {
    let mut title = TitleInput::new();
    let base = title.0.as_ptr() as u64;
    let word = if matches!(input, Input::StagedRoot) {
        4
    } else {
        0
    };
    let address = base + (word * 4) as u64;
    let shadow = shadow::find(address, 4).unwrap();
    let (output, readback) = host.shared_buffer(256).unwrap();
    let producer = host.new_list().unwrap();
    let reader = host.new_list().unwrap();
    let heap = HostHeap::new(false, 1).unwrap();
    let gpu = shadow.gpu(address);
    let writes_gpu = !matches!(order, Order::CpuAfterRecording);
    // SAFETY: the test owns each list and backing. Buffers promote from
    // COMMON and the producer returns its buffer to COMMON before reading.
    unsafe {
        readback.cast::<u32>().write(u32::MAX);
        if writes_gpu {
            shadow.refresh(&producer, address, 4);
            shadow.gpu_writes(&producer, address, 4);
            shaders[0].bind(&producer);
            producer.list.SetComputeRootUnorderedAccessView(0, gpu);
            producer.list.Dispatch(1, 1, 1);
            producer.list.ResourceBarrier(&[transition(
                &shadow.buffer,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
        }
        producer.close();
        match input {
            Input::AlignedRoot | Input::StagedRoot => {
                // Exercise the actual root CBV selection and staging code.
                let bound = root_view(&reader, address, false).unwrap();
                assert_eq!(bound == gpu, matches!(input, Input::AlignedRoot));
                shaders[1].bind(&reader);
                reader.list.SetComputeRootConstantBufferView(0, bound);
            }
            Input::StagedTable => {
                // Same scratch fill and native heap CBV as draw::tables.
                let (bound, scratch) = reader.scratch(256).unwrap();
                reader.defer_fill(scratch, address as *const u8, 256);
                let index = heap.dynamic(1).unwrap();
                heap.constant_at(index, bound, 256);
                reader.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
                shaders[2].bind(&reader);
                reader
                    .list
                    .SetComputeRootDescriptorTable(0, heap.gpu_handle(index));
            }
        }
        reader
            .list
            .SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
        reader.list.Dispatch(1, 1, 1);
        reader.close();
        match order {
            Order::CpuAfterRecording => {
                title.0[word] = 43;
                host.execute(&[&reader]);
            }
            Order::GpuSameSubmission => host.execute(&[&producer, &reader]),
            Order::GpuSeparateSubmissions => {
                host.execute(&[&producer]);
                host.execute(&[&reader]);
            }
        }
        (readback.cast::<u32>().read(), title.0[word])
    }
}

#[test]
#[ignore = "requires explicit D3D12 debug/GBV setup"]
fn material_constant_staging_observes_title_and_gpu_writes_at_distinct_boundaries() {
    let host = host().expect("fixture adapter");
    let configuration: ID3D12DeviceConfiguration = host.device.cast().unwrap();
    // SAFETY: a plain query of the active device configuration.
    let description = unsafe { configuration.GetDesc() };
    assert_ne!(
        description.Flags.0 & D3D12_DEVICE_FLAG_GPU_BASED_VALIDATION_ENABLED.0,
        0
    );
    assert_eq!(description.GpuBasedValidationFlags, 0);
    let info: ID3D12InfoQueue = host.device.cast().unwrap();
    let shaders = [
        Shader::new(host, s!("producer")),
        Shader::new(host, s!("direct_reader")),
        Shader::new(host, s!("table_reader")),
    ];
    let ordered_staging = std::env::var_os("VICTORY_MATERIAL_TEST_ORDERED_STAGING").is_some();
    for input in [Input::AlignedRoot, Input::StagedRoot, Input::StagedTable] {
        for order in [
            Order::CpuAfterRecording,
            Order::GpuSameSubmission,
            Order::GpuSeparateSubmissions,
        ] {
            let observed = run(host, &shaders, input, order);
            let expected = match (input, order) {
                (_, Order::CpuAfterRecording) => (43, 43),
                (Input::StagedRoot | Input::StagedTable, Order::GpuSameSubmission)
                    if !ordered_staging =>
                {
                    (11, 29)
                }
                _ => (29, 29),
            };
            assert_eq!(observed, expected, "{input:?} {order:?}");
            eprintln!("{input:?} {order:?}: consumer/CPU readback {observed:?}");
        }
    }
    // SAFETY: the debug queue writes each message to provided aligned storage.
    unsafe {
        for id in 0..info.GetNumStoredMessages() {
            let mut size = 0;
            info.GetMessage(id, None, &mut size).unwrap();
            let mut data = vec![0u64; size.div_ceil(8)];
            let message = data.as_mut_ptr().cast::<D3D12_MESSAGE>();
            info.GetMessage(id, Some(message), &mut size).unwrap();
            let message = &*message;
            assert!(
                message.Severity.0 > D3D12_MESSAGE_SEVERITY_ERROR.0,
                "D3D12: {}",
                std::ffi::CStr::from_ptr(message.pDescription.cast()).to_string_lossy()
            );
        }
    }
}
