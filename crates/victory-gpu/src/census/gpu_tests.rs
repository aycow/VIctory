use super::*;
use crate::host::{create_with_device, transition, warp_adapter};
use std::mem::ManuallyDrop;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCSTR, s};

const COMPUTE: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), UAV(u0)"
cbuffer Controls : register(b0) { uint index, rounds; }
RWByteAddressBuffer output : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main(uint3 group : SV_GroupID) {
    if (rounds == 0) {
        output.Store((index + group.x) * 4, 1);
        return;
    }
    for (uint i = 0; i < rounds; ++i) {
        uint previous;
        output.InterlockedAdd(index * 4, 1, previous);
    }
}
"#;

const MESH: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), UAV(u0)"
cbuffer Controls : register(b0) { uint index, rounds; }
RWByteAddressBuffer output : register(u0);
struct Vertex { float4 position : SV_Position; };
[RootSignature(RS)]
[outputtopology("triangle")]
[numthreads(1, 1, 1)]
void main(out vertices Vertex verts[3], out indices uint3 tris[1]) {
    SetMeshOutputCounts(3, 1);
    verts[0].position = float4(-1, -1, 0, 1);
    verts[1].position = float4(0, 1, 0, 1);
    verts[2].position = float4(1, -1, 0, 1);
    tris[0] = uint3(0, 1, 2);
    uint previous;
    output.InterlockedAdd(index * 4, rounds, previous);
}
"#;

fn adapter() -> IDXGIAdapter {
    let vendor = std::env::var("VICTORY_CENSUS_TEST_ADAPTER")
        .expect("set VICTORY_CENSUS_TEST_ADAPTER to amd, nvidia or warp");
    let vendor = match vendor.as_str() {
        "amd" => 0x1002,
        "nvidia" => 0x10de,
        "warp" => return warp_adapter().expect("WARP software adapter"),
        _ => panic!("unknown adapter {vendor}"),
    };
    // SAFETY: only querying live adapters and their descriptions.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        (0..)
            .map_while(|i| factory.EnumAdapters1(i).ok())
            .find(|adapter| adapter.GetDesc1().unwrap().VendorId == vendor)
            .expect("requested hardware adapter")
            .cast()
            .unwrap()
    }
}

fn pipeline_id(source: &str, stage: &'static str) -> Arc<Pipeline> {
    Arc::new(Pipeline {
        hash: blake3::hash(format!("{source}:{stage}").as_bytes()).to_hex()[..32].to_owned(),
        shaders: vec![(
            stage,
            blake3::hash(source.as_bytes()).to_hex()[..16].to_owned(),
        )],
    })
}

fn buffer(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
    size: u64,
    state: D3D12_RESOURCE_STATES,
    flags: D3D12_RESOURCE_FLAGS,
) -> ID3D12Resource {
    let mut buffer = None;
    // SAFETY: valid heap, buffer description, and initial state.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: heap,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: size,
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
                },
                state,
                None,
                &mut buffer,
            )
            .unwrap();
    }
    buffer.unwrap()
}

fn compile_compute(source: &str) -> Vec<u8> {
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: source and names live through compilation; the output is ours.
    unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("synthetic census"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            None,
        )
        .unwrap();
        let code = code.unwrap();
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
            .to_vec()
    }
}

fn compile_mesh() -> Vec<u8> {
    let compiler = std::env::var_os("VICTORY_CENSUS_TEST_DXC").expect("set path to dxc.exe");
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("census.hlsl");
    let output = directory.path().join("census.dxil");
    std::fs::write(&input, MESH).unwrap();
    let result = std::process::Command::new(compiler)
        .args(["-T", "ms_6_6", "-E", "main", "-Fo"])
        .arg(&output)
        .arg(&input)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::read(output).unwrap()
}

fn bytecode(bytes: &[u8]) -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE {
        pShaderBytecode: bytes.as_ptr().cast(),
        BytecodeLength: bytes.len(),
    }
}

fn compute_pipeline(
    device: &ID3D12Device,
    bytes: &[u8],
) -> (ID3D12RootSignature, ID3D12PipelineState) {
    // SAFETY: the bytecode contains a root signature and all borrowed inputs are live.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).unwrap();
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: bytecode(bytes),
            ..Default::default()
        };
        let pso = device.CreateComputePipelineState(&desc).unwrap();
        drop(ManuallyDrop::into_inner(desc.pRootSignature));
        (root, pso)
    }
}

fn mesh_pipeline(
    device: &ID3D12Device,
    bytes: &[u8],
) -> (ID3D12RootSignature, ID3D12PipelineState) {
    #[repr(C, align(8))]
    struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);
    #[repr(C)]
    struct Stream {
        root: Sub<*mut std::ffi::c_void>,
        ms: Sub<D3D12_SHADER_BYTECODE>,
        depth: Sub<D3D12_DEPTH_STENCIL_DESC>,
        samples: Sub<DXGI_SAMPLE_DESC>,
    }
    // SAFETY: the stream contains full, aligned subobjects with live data.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).unwrap();
        let mut stream = Stream {
            root: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
                root.as_raw(),
            ),
            ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, bytecode(bytes)),
            depth: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL,
                D3D12_DEPTH_STENCIL_DESC {
                    DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                    ..Default::default()
                },
            ),
            samples: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC,
                DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
            ),
        };
        let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: size_of::<Stream>(),
            pPipelineStateSubobjectStream: (&raw mut stream).cast(),
        };
        let pso = device
            .cast::<ID3D12Device2>()
            .unwrap()
            .CreatePipelineState(&desc)
            .unwrap();
        (root, pso)
    }
}

fn test_device(gbv: bool) -> (ID3D12Device, String) {
    // SAFETY: This test configures validation before creating any device.
    unsafe {
        let sdk = std::env::var("VICTORY_CENSUS_TEST_AGILITY")
            .expect("set path to the Agility SDK 1.620.x runtime");
        let sdk =
            std::ffi::CString::new(format!("{}\\", sdk.trim_end_matches(['/', '\\']))).unwrap();
        let mut config: Option<ID3D12SDKConfiguration1> = None;
        D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut config).unwrap();
        let factory: ID3D12DeviceFactory = config
            .unwrap()
            .CreateDeviceFactory(620, PCSTR(sdk.as_ptr().cast()))
            .expect("load test Agility SDK");
        let debug: ID3D12Debug = factory
            .GetConfigurationInterface(&CLSID_D3D12Debug)
            .expect("Agility debug layer");
        debug.EnableDebugLayer();
        debug
            .cast::<ID3D12Debug1>()
            .unwrap()
            .SetEnableGPUBasedValidation(gbv);
        let adapter = adapter();
        let desc = adapter.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap();
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..end]);
        let mut device: Option<ID3D12Device> = None;
        factory
            .CreateDevice(
                &adapter,
                windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0,
                &mut device,
            )
            .expect("create test D3D12 device");
        (device.expect("test D3D12 device"), name)
    }
}

#[test]
#[ignore = "requires a selected D3D12 adapter, Graphics Tools and DXC"]
fn census_gpu_smoke() {
    let profile = enabled();
    let gbv = std::env::var_os("VICTORY_CENSUS_TEST_DISABLE_GBV").is_none();
    let multi = std::env::var_os("VICTORY_CENSUS_TEST_MULTI_INDIRECT").is_some();
    let limit = if multi { 3 } else { 1 };
    let zero_count = std::env::var_os("VICTORY_CENSUS_TEST_ZERO_INDIRECT").is_some();
    let count = if zero_count {
        0
    } else if multi {
        2
    } else {
        1
    };
    assert!(
        std::env::var_os("VICTORY_D3D12_DEBUG").is_none(),
        "test enables full GBV itself"
    );
    // SAFETY: validation is enabled before the first device is created in
    // this test. Every COM object and CPU mapping below is kept alive until
    // the submission finishes.
    unsafe {
        let (device, name) = test_device(gbv);
        let host = create_with_device(device, false).expect("initialize test host");
        let device = &host.device;
        assert!(
            host.census_frequency().is_some(),
            "direct queue has no timestamp frequency"
        );
        let messages = device.cast::<ID3D12InfoQueue>().unwrap();
        let use_mesh = host.mesh_shaders();

        // Three records (2, 3 and 5 groups), plus the GPU count.
        let mut upload_data = [0_u32; 45];
        upload_data[32..].copy_from_slice(&[2, 1, 1, 0, 3, 1, 1, 0, 5, 1, 1, 0, count]);
        let upload = buffer(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            size_of_val(&upload_data) as u64,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            D3D12_RESOURCE_FLAG_NONE,
        );
        let output = buffer(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            128,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        );
        let arguments = buffer(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            52,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_FLAG_NONE,
        );
        let readback = buffer(
            device,
            D3D12_HEAP_TYPE_READBACK,
            128,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_FLAG_NONE,
        );
        let mut data = std::ptr::null_mut();
        upload
            .Map(0, Some(&D3D12_RANGE::default()), Some(&mut data))
            .unwrap();
        data.cast::<u32>()
            .copy_from_nonoverlapping(upload_data.as_ptr(), upload_data.len());
        upload.Unmap(0, None);

        let (root, pso) = compute_pipeline(device, &compile_compute(COMPUTE));
        let id = pipeline_id(COMPUTE, "compute");
        let argument = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH,
            ..Default::default()
        };
        let mut signature: Option<ID3D12CommandSignature> = None;
        device
            .CreateCommandSignature(
                &D3D12_COMMAND_SIGNATURE_DESC {
                    ByteStride: 16,
                    NumArgumentDescs: 1,
                    pArgumentDescs: &argument,
                    NodeMask: 0,
                },
                None,
                &mut signature,
            )
            .unwrap();
        let signature = signature.unwrap();

        let alu = host.new_list().unwrap();
        let list = &alu.list;
        list.ResourceBarrier(&[
            transition(
                &output,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            ),
            transition(
                &arguments,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            ),
        ]);
        list.CopyBufferRegion(&output, 0, &upload, 0, 128);
        list.CopyBufferRegion(&arguments, 0, &upload, 128, 52);
        list.ResourceBarrier(&[transition(
            &arguments,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
        )]);
        list.ResourceBarrier(&[transition(
            &output,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
        list.SetComputeRootSignature(&root);
        list.SetPipelineState(&pso);
        list.SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
        for (groups, rounds) in [(4, 1), (16, 32)] {
            let controls = [0_u32, rounds];
            list.SetComputeRoot32BitConstants(0, 2, controls.as_ptr().cast(), 0);
            let sample =
                profile.then(|| alu.census.begin(list, &id, Work::Dispatch([groups, 1, 1])));
            list.Dispatch(groups, 1, 1);
            drop(sample);
        }
        alu.close();

        let indirect = host.new_list().unwrap();
        let list = &indirect.list;
        list.SetComputeRootSignature(&root);
        list.SetPipelineState(&pso);
        list.SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
        for (index, counted) in [(8_u32, false), (16_u32, true)] {
            let controls = [index, 0_u32];
            list.SetComputeRoot32BitConstants(0, 2, controls.as_ptr().cast(), 0);
            let sample = profile.then(|| {
                indirect.census.begin(
                    list,
                    &id,
                    Work::Indirect {
                        kind: 2,
                        limit,
                        stride: 16,
                        counted,
                    },
                )
            });
            list.ExecuteIndirect(
                &signature,
                limit,
                &arguments,
                0,
                counted.then_some(&arguments),
                if counted { 48 } else { 0 },
            );
            drop(sample);
        }

        let mesh = use_mesh.then(|| mesh_pipeline(device, &compile_mesh()));
        let mid = pipeline_id(MESH, "mesh");
        if let Some((root, pso)) = &mesh {
            list.SetGraphicsRootSignature(root);
            list.SetPipelineState(pso);
            let controls = [3_u32, 5];
            list.SetGraphicsRoot32BitConstants(0, 2, controls.as_ptr().cast(), 0);
            list.SetGraphicsRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
            let sample = profile.then(|| indirect.census.begin(list, &mid, Work::Mesh([4, 1, 1])));
            list.cast::<ID3D12GraphicsCommandList6>()
                .unwrap()
                .DispatchMesh(4, 1, 1);
            drop(sample);
        }
        list.ResourceBarrier(&[transition(
            &output,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.CopyBufferRegion(&readback, 0, &output, 0, 128);
        indirect.close();
        host.execute(&[&alu, &indirect]);
        host.drain();
        assert_eq!(
            host.submission_count(),
            1,
            "census added or split a submission"
        );

        readback.Map(0, None, Some(&mut data)).unwrap();
        let words = std::slice::from_raw_parts(data.cast::<u32>(), 32).to_vec();
        readback.Unmap(0, Some(&D3D12_RANGE::default()));
        let mut reference = [0_u32; 32];
        reference[0] = 4 + 16 * 32;
        reference[3] = if use_mesh { 20 } else { 0 };
        reference[8..if multi { 13 } else { 10 }].fill(1);
        match count {
            2 => reference[16..19].fill(1),
            1 => reference[16..18].fill(1),
            _ => (),
        }
        let warns = validation_messages(&messages);
        assert!(warns.is_empty(), "{name}: outputs {words:?}, {warns:#?}");
        eprintln!(
            "{name}: census={profile}, GBV={gbv}, limit={limit}, count={count}, outputs {words:?}"
        );
        assert_eq!(words, reference, "{name}");
        if !profile {
            return;
        }
        let mut totals = with_totals(std::mem::take);
        let direct = totals.execution.remove(&(id.clone(), "compute")).unwrap();
        assert_eq!(
            (direct.calls, direct.total_items, direct.max_items),
            (2, 20, 16)
        );
        assert_eq!(direct.timings.ns.len(), 2);
        assert!(direct.timings.total_ns > 0);
        let indirect_entry = totals.execution.remove(&(id, "indirect_compute")).unwrap();
        assert_eq!(
            (
                indirect_entry.calls,
                indirect_entry.fixed_indirect_records,
                indirect_entry.counted_indirect_calls,
                indirect_entry.counted_indirect_limit
            ),
            (2, u64::from(limit), 1, u64::from(limit))
        );
        assert_eq!(indirect_entry.timings.ns.len(), 2);
        assert!(indirect_entry.timings.total_ns > 0);
        let mesh_ns = if use_mesh {
            let entry = totals.execution.remove(&(mid, "mesh")).unwrap();
            assert_eq!(
                (entry.calls, entry.total_items, entry.timings.ns.len()),
                (1, 4, 1)
            );
            assert!(entry.timings.total_ns > 0);
            entry.timings.ns
        } else {
            Vec::new()
        };
        assert!(totals.execution.is_empty());

        eprintln!(
            "{name}: 1 submission, direct ns {:?}, indirect ns {:?}, mesh ns {mesh_ns:?}",
            direct.timings.ns, indirect_entry.timings.ns
        );
    }
}

fn validation_messages(messages: &ID3D12InfoQueue) -> Vec<String> {
    // SAFETY: the info queue writes each message into a block of its requested size.
    unsafe {
        let mut warns = Vec::new();
        for i in 0..messages.GetNumStoredMessages() {
            let mut size = 0;
            messages.GetMessage(i, None, &mut size).unwrap();
            let mut storage = vec![0u64; size.div_ceil(8)];
            let msg = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
            messages.GetMessage(i, Some(msg), &mut size).unwrap();
            if (*msg).Severity.0 <= D3D12_MESSAGE_SEVERITY_WARNING.0 {
                let bytes =
                    std::slice::from_raw_parts((*msg).pDescription, (*msg).DescriptionByteLength);
                let text = String::from_utf8_lossy(bytes);
                // Agility 620 emits this advisory for the host's feature queries.
                if (*msg).Severity != D3D12_MESSAGE_SEVERITY_WARNING
                    || !text.starts_with("ID3D12Device::CheckFeatureSupport is superseded")
                {
                    warns.push(text.into_owned());
                }
            }
        }
        warns
    }
}

#[test]
#[ignore = "requires a named D3D12 adapter and Agility SDK 1.620.x"]
fn reset_and_resubmit_preserves_pending_census_generations() {
    use crate::host::{HostList, test_install_device};

    assert!(enabled(), "set VICTORY_GPU_CENSUS=1");
    let (device, name) = test_device(true);
    let info: ID3D12InfoQueue = device.cast().unwrap();
    let host = test_install_device(device.clone());
    let (root, pso) = compute_pipeline(&device, &compile_compute(COMPUTE));
    let outputs = [(); 2].map(|()| {
        buffer(
            &device,
            D3D12_HEAP_TYPE_DEFAULT,
            256,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        )
    });
    let first = pipeline_id("synthetic first recording", "cs");
    let second = pipeline_id("synthetic second recording", "cs");
    let record = |list: &HostList, id: &Arc<Pipeline>, target: usize, sizes: &[u32]| {
        // SAFETY: the fixture holds the PSO, root signature and output through every fence.
        unsafe {
            list.list.SetComputeRootSignature(&root);
            list.list.SetPipelineState(&pso);
            list.list
                .SetComputeRootUnorderedAccessView(1, outputs[target].GetGPUVirtualAddress());
            for (i, &x) in sizes.iter().enumerate() {
                let values = [32 * i as u32, 0];
                list.list
                    .SetComputeRoot32BitConstants(0, 2, values.as_ptr().cast(), 0);
                let sample = list.census.begin(&list.list, id, Work::Dispatch([x, 1, 1]));
                list.list.Dispatch(x, 1, 1);
                drop(sample);
            }
        }
        list.close();
    };
    with_totals(std::mem::take);
    let list = host.new_list().unwrap();
    record(&list, &first, 0, &[4]);
    let old_query = {
        let old = list.census.snapshot().unwrap();
        old.queries.get().unwrap().as_ref().unwrap().0.as_raw() as usize
    };

    // SAFETY: this test owns the fence and only gates its private host.
    let gate: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.unwrap();
    let (done, wake) = std::sync::mpsc::channel();
    let release = gate.clone();
    let watchdog = std::thread::spawn(move || {
        let released = wake.recv_timeout(Duration::from_secs(3)).is_ok();
        // SAFETY: signal the fixture's private fence even after a fixture failure.
        unsafe { release.Signal(1) }.unwrap();
        released
    });
    host.execute_gated(&[&list], &gate);
    assert!(
        list.running(),
        "the gated recording must still be in flight"
    );
    list.reset();
    record(&list, &second, 1, &[8, 16]);
    let new = list.census.snapshot().unwrap();
    let new_query = new.queries.get().unwrap().as_ref().unwrap().0.as_raw() as usize;
    assert_ne!(old_query, new_query, "reset must detach pending queries");
    drop(new);

    host.execute(&[&list]);
    let hold = Duration::from_millis(100);
    std::thread::sleep(hold);
    assert!(with_totals(|totals| totals.execution.is_empty()));
    done.send(()).unwrap();
    assert!(watchdog.join().unwrap());
    host.drain();

    host.execute(&[&list]);
    host.drain();
    let mut totals = with_totals(std::mem::take);
    let a = totals.execution.remove(&(first, "compute")).unwrap();
    let b = totals.execution.remove(&(second, "compute")).unwrap();
    assert!(totals.execution.is_empty());
    assert_eq!((a.calls, a.total_items, a.timings.ns.len()), (1, 4, 1));
    assert_eq!((b.calls, b.total_items, b.timings.ns.len()), (4, 48, 4));
    assert!(a.timings.total_ns < hold.as_nanos());
    assert!(b.timings.total_ns < hold.as_nanos());
    assert_eq!(host.submission_count(), 3, "census added an execution");
    let warnings = validation_messages(&info);
    assert!(warnings.is_empty(), "{warnings:#?}");
    eprintln!(
        "{name}: held native fence, GPU spans {:?} and {:?}",
        a.timings.ns, b.timings.ns
    );
}
