use super::*;
use crate::host::{Host, HostList, create_with_device, transition, warp_adapter};
use std::mem::ManuallyDrop;
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, Fxc::D3DCompile, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCSTR, s};

const SHADER: &str = r#"
Texture2D<float4> source : register(t0);
RWTexture2D<float4> destination : register(u0);

[RootSignature("DescriptorTable(SRV(t0), UAV(u0))")]
[numthreads(1, 1, 1)]
void main() {
    destination[uint2(0, 0)] = source.Load(int3(0, 0, 0));
}
"#;

fn adapter(kind: &str) -> IDXGIAdapter {
    let vendor = match kind {
        "amd" => 0x1002,
        "nvidia" => 0x10de,
        "warp" => return warp_adapter().unwrap(),
        _ => panic!("unknown adapter {kind}"),
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

fn texture_2d(
    device: &ID3D12Device,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
    color: Option<[f32; 4]>,
) -> ID3D12Resource {
    let mut resource = None;
    let clear = color.map(|color| D3D12_CLEAR_VALUE {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        Anonymous: D3D12_CLEAR_VALUE_0 { Color: color },
    });
    // SAFETY: valid default heap texture and compatible initial state.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: 1,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Flags: flags,
                    ..Default::default()
                },
                state,
                clear.as_ref().map(std::ptr::from_ref),
                &mut resource,
            )
            .unwrap();
    }
    resource.unwrap()
}

fn pipeline(device: &ID3D12Device) -> (ID3D12RootSignature, ID3D12PipelineState) {
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: the source, names, and returned shader blob are live in these calls.
    unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("synthetic render dependencies"),
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
        let bytes =
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).unwrap();
        let pso = device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: ManuallyDrop::new(Some(root.clone())),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })
            .unwrap();
        (root, pso)
    }
}

fn record_state(
    list: &HostList,
    texture: &ID3D12Resource,
    resource: Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    // SAFETY: open direct list and live texture. Callers supply the current state.
    unsafe {
        list.list
            .ResourceBarrier(&[transition(texture, before, after)]);
    }
    list.note_dependency(|| Event::State {
        resource,
        before: before.0 as u32,
        after: after.0 as u32,
        prelude: false,
    });
}

fn render(host: &Host, frame: u64, producer: bool) {
    let device = &host.device;
    let target_id = frame * 10;
    let final_id = target_id + 1;
    let targets = D3D12_RESOURCE_STATE_RENDER_TARGET;
    let shader_read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
    let shader_write = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
    let copy_source = D3D12_RESOURCE_STATE_COPY_SOURCE;
    let black = [0., 0., 0., 1.];
    let red = [1., 0., 0., 1.];
    let intermediate = texture_2d(
        device,
        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        targets,
        Some(black),
    );
    let final_image = texture_2d(
        device,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        shader_write,
        None,
    );
    let producer_texture = producer.then(|| {
        texture_2d(
            device,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            targets,
            Some(red),
        )
    });
    let (root, pso) = pipeline(device);
    // SAFETY: objects and descriptor heaps stay live through their fence wait.
    unsafe {
        let rtv_heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 2,
                ..Default::default()
            })
            .unwrap();
        let rtv = rtv_heap.GetCPUDescriptorHandleForHeapStart();
        let producer_rtv = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: rtv.ptr
                + device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize,
        };
        device.CreateRenderTargetView(&intermediate, None, rtv);
        let shader_heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 2,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                ..Default::default()
            })
            .unwrap();
        let srv = shader_heap.GetCPUDescriptorHandleForHeapStart();
        let uav = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: srv.ptr
                + device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                    as usize,
        };
        device.CreateShaderResourceView(&intermediate, None, srv);
        device.CreateUnorderedAccessView(&final_image, None, None, uav);

        let seed = host.new_list().unwrap();
        seed.list.ClearRenderTargetView(rtv, &black, None);
        seed.note_dependency(|| {
            Event::transfer("seed_black", vec![access(target_id, Role::Clear)])
        });
        seed.note_dependency(|| present(frame - 1, target_id));
        seed.close();
        host.execute(&[&seed]);

        // Record the consumer first, then submit the producer first. The
        // fixture uses an offscreen source to avoid opening a window.
        let consumer = host.new_list().unwrap();
        consumer.list.SetPipelineState(&pso);
        consumer.list.SetComputeRootSignature(&root);
        consumer
            .list
            .SetDescriptorHeaps(&[Some(shader_heap.clone())]);
        consumer
            .list
            .SetComputeRootDescriptorTable(0, shader_heap.GetGPUDescriptorHandleForHeapStart());
        consumer.list.Dispatch(1, 1, 1);
        let mut read = access(target_id, Role::Srv);
        read.binding = Some(Binding {
            stage: "compute",
            root: 0,
            offset_dwords: 0,
            host_slot: Some(0),
            title_resource: Some(target_id),
            title_view: "texture",
            host_resource_matches: Some(true),
        });
        consumer.note_dependency(|| Event::Pass {
            kind: "dispatch",
            key: "synthetic".into(),
            shaders: "synthetic compute".into(),
            refusal: None,
            accesses: vec![read, access(final_id, Role::Uav)],
        });
        record_state(&consumer, &final_image, final_id, shader_write, copy_source);
        let (readback, _) =
            crate::capture::read_back(device, &consumer.list, &final_image).unwrap();
        consumer.note_dependency(|| present(frame, final_id));
        consumer.close();

        let prepare = host.new_list().unwrap();
        let mut intermediate_state = targets;
        if let Some(texture) = producer_texture.as_ref() {
            let producer_id = target_id + 2;
            device.CreateRenderTargetView(texture, None, producer_rtv);
            prepare.list.ClearRenderTargetView(producer_rtv, &red, None);
            prepare.note_dependency(|| {
                Event::transfer("clear_red", vec![access(producer_id, Role::Clear)])
            });
            let copy_dest = D3D12_RESOURCE_STATE_COPY_DEST;
            record_state(&prepare, texture, producer_id, targets, copy_source);
            record_state(&prepare, &intermediate, target_id, targets, copy_dest);
            prepare.list.CopyResource(&intermediate, texture);
            prepare.note_dependency(|| {
                Event::transfer(
                    "copy_red",
                    vec![
                        access(producer_id, Role::CopySource),
                        access(target_id, Role::CopyDest),
                    ],
                )
            });
            intermediate_state = copy_dest;
        }
        record_state(
            &prepare,
            &intermediate,
            target_id,
            intermediate_state,
            shader_read,
        );
        prepare.close();
        let report = capture(|| host.execute(&[&prepare, &consumer]))
            .pop()
            .unwrap();
        let dispatch = report
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    &node.event,
                    Event::Pass {
                        kind: "dispatch",
                        ..
                    }
                )
            })
            .unwrap();
        let source = dispatch
            .inputs
            .iter()
            .find(|input| matches!(input.access.role, Role::Srv))
            .unwrap();
        let writer = source.writer.unwrap();
        assert_eq!(source.state, Some(shader_read.0 as u32));
        assert_eq!(writer.frame, if producer { frame } else { frame - 1 });
        assert!(report.lineage().contains(&writer.sequence));
        assert!(report.lineage().contains(&dispatch.point.sequence));
        assert!(source.refused_writer.is_none());
        assert_eq!(report.omitted, 0);
        let mut pixel = std::ptr::null_mut();
        readback
            .Map(0, Some(&D3D12_RANGE { Begin: 0, End: 4 }), Some(&mut pixel))
            .unwrap();
        let actual = pixel.cast::<[u8; 4]>().read();
        readback.Unmap(0, Some(&D3D12_RANGE::default()));
        assert_eq!(actual, [if producer { 255 } else { 0 }, 0, 0, 255]);
        println!(
            "producer={producer} frame={frame} source_writer_frame={} pixel={actual:?}",
            writer.frame
        );
    }
}

#[test]
#[ignore = "requires Windows Agility SDK 1.620.x and an explicit D3D12 adapter"]
fn synthetic_gpu() {
    assert_eq!(
        window(),
        Some((1, 4)),
        "set VICTORY_RENDER_DEPENDENCIES=1-4"
    );
    let vendor = std::env::var("VICTORY_RENDER_TEST_ADAPTER").expect("set amd, nvidia or warp");
    let sdk = std::env::var("VICTORY_RENDER_TEST_AGILITY").expect("set path to Agility runtime");
    let sdk = std::ffi::CString::new(format!("{}\\", sdk.trim_end_matches(['/', '\\']))).unwrap();
    // SAFETY: validation is enabled before creating the device and object
    // lifetimes extend through submissions. Message storage is sized by D3D12.
    unsafe {
        let mut configuration: Option<ID3D12SDKConfiguration1> = None;
        D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut configuration).unwrap();
        let factory: ID3D12DeviceFactory = configuration
            .unwrap()
            .CreateDeviceFactory(620, PCSTR(sdk.as_ptr().cast()))
            .unwrap();
        let debug: ID3D12Debug = factory
            .GetConfigurationInterface(&CLSID_D3D12Debug)
            .unwrap();
        debug.EnableDebugLayer();
        debug
            .cast::<ID3D12Debug1>()
            .unwrap()
            .SetEnableGPUBasedValidation(true);
        let adapter = adapter(&vendor);
        let desc = adapter.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap();
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..end]);
        let mut device: Option<ID3D12Device> = None;
        factory
            .CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)
            .unwrap();
        let host = create_with_device(device.unwrap(), false).unwrap();
        let messages = host.device.cast::<ID3D12InfoQueue>().unwrap();
        render(&host, 2, true);
        render(&host, 4, false);
        let mut failures = Vec::new();
        for i in 0..messages.GetNumStoredMessages() {
            let mut size = 0;
            messages.GetMessage(i, None, &mut size).unwrap();
            let mut storage = vec![0u64; size.div_ceil(8)];
            let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
            messages.GetMessage(i, Some(message), &mut size).unwrap();
            if (*message).Severity.0 <= D3D12_MESSAGE_SEVERITY_WARNING.0 {
                let bytes = std::slice::from_raw_parts(
                    (*message).pDescription,
                    (*message).DescriptionByteLength,
                );
                failures.push(String::from_utf8_lossy(bytes).into_owned());
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
        println!("{name}, D3D12 debug layer and full GPU-based validation, 0 warnings/errors");
    }
}
