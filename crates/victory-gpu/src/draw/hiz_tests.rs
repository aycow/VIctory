//! Original placed-depth, Hi-Z, culling and indirect mesh validation.

use super::{HostList, State};
use crate::descriptor::{self, HeapDesc};
use crate::host::{Host, install_hiz_test_device, transition};
use crate::object::{S_OK, This, guid, slot};
use crate::resource::{self, ResourceDesc};
use std::ffi::CString;
use std::mem::ManuallyDrop;
use victory_shader::{ResourceType, RootType, UsedResource};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCSTR};
use windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS;

const SOURCE: &str = include_str!("hiz_fixture.hlsl");
const WIDTH: usize = 128;
const HEIGHT: usize = 72;
const COPY_SOURCE: u32 = D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32;

struct TitleMemory {
    sampled: MEMORY_MAPPED_VIEW_ADDRESS,
    alias: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl TitleMemory {
    fn new(len: usize) -> Self {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
        };
        // SAFETY: create a private pagefile-backed section and two views of
        // the same complete range. The views keep the section alive.
        unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                len.try_into().unwrap(),
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let sampled = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len);
            let alias = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len);
            CloseHandle(section);
            let mapped = Self {
                sampled,
                alias,
                len,
            };
            assert!(!mapped.sampled.Value.is_null() && !mapped.alias.Value.is_null());
            assert_ne!(mapped.sampled.Value, mapped.alias.Value);
            mapped
        }
    }

    fn bytes(&mut self, through_alias: bool) -> &mut [u8] {
        let view = if through_alias {
            self.alias
        } else {
            self.sampled
        };
        // SAFETY: both views map len bytes, and the exclusive receiver
        // keeps the fixture from taking a slice of the other view at once.
        unsafe { std::slice::from_raw_parts_mut(view.Value.cast(), self.len) }
    }
}

impl Drop for TitleMemory {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        for view in [self.sampled, self.alias] {
            if !view.Value.is_null() {
                // SAFETY: each view is owned by the fixture.
                unsafe { UnmapViewOfFile(view) };
            }
        }
    }
}

fn device() -> (ID3D12Device, String) {
    let sdk = std::env::var("VICTORY_HIZ_TEST_AGILITY").expect("Agility 1.620.x x64 DLL path");
    let sdk = CString::new(format!("{}\\", sdk.trim_end_matches(['/', '\\']))).unwrap();
    let adapter = std::env::var("VICTORY_HIZ_TEST_ADAPTER").expect("amd, nvidia or warp");
    // SAFETY: the SDK directory, adapter enumeration and output pointers are live.
    unsafe {
        let mut configuration: Option<ID3D12SDKConfiguration1> = None;
        D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut configuration).unwrap();
        let factory: ID3D12DeviceFactory = configuration
            .unwrap()
            .CreateDeviceFactory(620, PCSTR(sdk.as_ptr().cast()))
            .unwrap();
        let debug: ID3D12Debug1 = factory
            .GetConfigurationInterface::<ID3D12Debug>(&CLSID_D3D12Debug)
            .unwrap()
            .cast()
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

fn compile(entry: &str, target: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("hiz.hlsl");
    let output = dir.path().join("hiz.dxil");
    std::fs::write(&source, SOURCE).unwrap();
    let result = std::process::Command::new(std::env::var_os("VICTORY_HIZ_TEST_DXC").unwrap())
        .args(["-E", entry, "-T", target, "-Fo"])
        .arg(&output)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{entry}: {}",
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

struct Compute(ID3D12RootSignature, ID3D12PipelineState);

impl Compute {
    fn new(device: &ID3D12Device, entry: &str) -> Self {
        let bytes = compile(entry, "cs_6_6");
        // SAFETY: DXC output contains the original shader and embedded root signature.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &bytes).unwrap();
            let pipeline = device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: ManuallyDrop::new(Some(root.clone())),
                    CS: bytecode(&bytes),
                    ..Default::default()
                })
                .unwrap();
            Self(root, pipeline)
        }
    }

    fn record(
        &self,
        list: &HostList,
        heap: &ID3D12DescriptorHeap,
        arguments: &ID3D12Resource,
        slots: [u32; 2],
        groups: [u32; 2],
    ) {
        // SAFETY: the pipeline, heap and argument buffer outlive the closed list.
        unsafe {
            list.list.SetDescriptorHeaps(&[Some(heap.clone())]);
            list.list.SetComputeRootSignature(&self.0);
            list.list.SetPipelineState(&self.1);
            list.list
                .SetComputeRoot32BitConstants(0, 2, slots.as_ptr().cast(), 0);
            list.list
                .SetComputeRootUnorderedAccessView(1, arguments.GetGPUVirtualAddress());
            list.list.Dispatch(groups[0], groups[1], 1);
        }
    }
}

struct TitleHeap(This);

impl TitleHeap {
    fn new() -> Self {
        #[repr(C)]
        struct Extended {
            words: [u32; 4],
            address: u64,
            size: u64,
        }
        let desc = Extended {
            words: [0, 4, 1, 0],
            address: 0,
            size: 0,
        };
        let iid = guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        let mut heap = std::ptr::null();
        assert_eq!(
            descriptor::create_heap(
                std::ptr::null(),
                (&raw const desc).cast::<HeapDesc>(),
                &iid,
                &mut heap
            ),
            S_OK
        );
        Self(heap)
    }

    fn at(&self, index: u32) -> u64 {
        descriptor::heap(self.0).start + u64::from(index) * 32
    }

    fn native(&self) -> &ID3D12DescriptorHeap {
        &descriptor::heap(self.0).host().unwrap().heap
    }

    fn prepare(&self, list: &HostList, bindings: &[(u32, ResourceType)]) {
        let mut state = State::default();
        state.set_heaps(&[self.0]);
        state.set_table(true, 0, self.at(0));
        let used: Vec<_> = bindings
            .iter()
            .map(|&(index, kind)| UsedResource {
                kind,
                root: RootType::Table,
                table_offset_dwords: index * 8,
                lower_bound: 0,
                stride: 0,
                dwords: 8,
                user_data: 0,
                slot: 0,
            })
            .collect();
        assert!(state.tables(list, &used, true).is_empty());
        assert!(!state.table_failed);
    }
}

impl Drop for TitleHeap {
    fn drop(&mut self) {
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(self.0, 0x10)(self.0),
            0
        );
    }
}

fn texture_desc(
    w: u64,
    h: u32,
    f: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: w,
        Height: h,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: f,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: flags,
        ..Default::default()
    }
}

fn title_texture(width: u32, height: u32) -> This {
    let desc = texture_desc(
        width.into(),
        height,
        DXGI_FORMAT_R32_FLOAT,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
    );
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
        ..Default::default()
    };
    let iid = guid(
        0x696442be,
        0xa72e,
        0x4059,
        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
    );
    let mut texture = std::ptr::null();
    assert_eq!(
        resource::create_committed(
            std::ptr::null(),
            (&raw const heap).cast(),
            0,
            (&raw const desc).cast(),
            0,
            std::ptr::null(),
            &iid,
            &mut texture,
        ),
        S_OK
    );
    texture
}

fn native_resource(
    host: &Host,
    kind: D3D12_HEAP_TYPE,
    desc: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
    clear: Option<&D3D12_CLEAR_VALUE>,
) -> ID3D12Resource {
    let mut resource = None;
    // SAFETY: the heap and resource descriptions are valid, and the output is live.
    unsafe {
        host.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: kind,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                desc,
                state,
                clear.map(std::ptr::from_ref),
                &mut resource,
            )
            .unwrap()
    };
    resource.unwrap()
}

fn buffer_desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
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
    }
}

fn read_texture(host: &Host, texture: This) -> Vec<u32> {
    let list = host.new_list().unwrap();
    crate::list::ensure_state(&list, texture, COPY_SOURCE);
    let resource = resource::host_resource(texture).unwrap();
    let (readback, footprint) =
        crate::capture::read_back(&host.device, &list.list, resource).unwrap();
    list.close();
    host.execute(&[&list]);
    texture_words(&readback, footprint)
}

fn texture_words(
    readback: &ID3D12Resource,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) -> Vec<u32> {
    let f = footprint.Footprint;
    let mut words = Vec::with_capacity(f.Width as usize * f.Height as usize);
    let mut mapped = std::ptr::null_mut();
    // SAFETY: the fixture waits for this copy, and each row names mapped pixels.
    unsafe {
        readback.Map(0, None, Some(&mut mapped)).unwrap();
        for row in 0..f.Height as usize {
            let data = mapped
                .cast::<u8>()
                .add(footprint.Offset as usize + row * f.RowPitch as usize);
            words.extend_from_slice(std::slice::from_raw_parts(
                data.cast::<u32>(),
                f.Width as usize,
            ));
        }
        readback.Unmap(0, Some(&D3D12_RANGE::default()));
    }
    words
}

fn stats(label: &str, data: &[u32]) {
    let min = data.iter().copied().min().unwrap();
    let max = data.iter().copied().max().unwrap();
    let nonzero = data.iter().filter(|&&word| word != 0).count();
    eprintln!(
        "{label}: min={} max={} nonzero={nonzero}/{}",
        f32::from_bits(min),
        f32::from_bits(max),
        data.len()
    );
}

fn validate_queue(info: &ID3D12InfoQueue) {
    let mut issues = Vec::new();
    // SAFETY: each message buffer has the size and alignment the queue requests.
    unsafe {
        for i in 0..info.GetNumStoredMessages() {
            let mut size = 0;
            info.GetMessage(i, None, &mut size).unwrap();
            let mut bytes = vec![0u64; size.div_ceil(8)];
            let message = bytes.as_mut_ptr().cast::<D3D12_MESSAGE>();
            info.GetMessage(i, Some(message), &mut size).unwrap();
            let message = &*message;
            if message.Severity.0 <= D3D12_MESSAGE_SEVERITY_WARNING.0 {
                issues.push(
                    std::ffi::CStr::from_ptr(message.pDescription.cast())
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    assert!(issues.is_empty(), "{issues:?}");
}

struct Mesh(
    ID3D12RootSignature,
    ID3D12PipelineState,
    ID3D12CommandSignature,
);

fn mesh_supported(device: &ID3D12Device) -> bool {
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
    // SAFETY: feature query writes the supplied complete options structure.
    let support = unsafe {
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS7,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .is_ok()
    };
    support && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
}

impl Mesh {
    fn new(device: &ID3D12Device) -> Self {
        let ms = compile("mesh", "ms_6_6");
        let ps = compile("pixel", "ps_6_6");
        // SAFETY: original DXIL bytecode, live stream subobjects and a three-dword mesh signature.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &ms).unwrap();
            #[repr(C, align(8))]
            struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);
            #[repr(C)]
            struct Stream {
                root: Sub<*mut std::ffi::c_void>,
                ms: Sub<D3D12_SHADER_BYTECODE>,
                ps: Sub<D3D12_SHADER_BYTECODE>,
                blend: Sub<D3D12_BLEND_DESC>,
                raster: Sub<D3D12_RASTERIZER_DESC>,
                depth: Sub<D3D12_DEPTH_STENCIL_DESC>,
                sample_mask: Sub<u32>,
                rtv: Sub<D3D12_RT_FORMAT_ARRAY>,
                samples: Sub<DXGI_SAMPLE_DESC>,
            }
            let mut blend = D3D12_BLEND_DESC::default();
            blend.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
                SrcBlend: D3D12_BLEND_ONE,
                DestBlend: D3D12_BLEND_ZERO,
                BlendOp: D3D12_BLEND_OP_ADD,
                SrcBlendAlpha: D3D12_BLEND_ONE,
                DestBlendAlpha: D3D12_BLEND_ZERO,
                BlendOpAlpha: D3D12_BLEND_OP_ADD,
                LogicOp: D3D12_LOGIC_OP_NOOP,
                RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
                ..Default::default()
            };
            let mut rtv = D3D12_RT_FORMAT_ARRAY {
                NumRenderTargets: 1,
                ..Default::default()
            };
            rtv.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
            let mut stream = Stream {
                root: Sub(
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
                    root.as_raw(),
                ),
                ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, bytecode(&ms)),
                ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, bytecode(&ps)),
                blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, blend),
                raster: Sub(
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER,
                    D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: D3D12_CULL_MODE_NONE,
                        DepthClipEnable: true.into(),
                        ..Default::default()
                    },
                ),
                depth: Sub(
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL,
                    D3D12_DEPTH_STENCIL_DESC {
                        DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                        DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                        ..Default::default()
                    },
                ),
                sample_mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, u32::MAX),
                rtv: Sub(
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
                    rtv,
                ),
                samples: Sub(
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC,
                    DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                ),
            };
            let pipeline = device
                .cast::<ID3D12Device2>()
                .unwrap()
                .CreatePipelineState(&D3D12_PIPELINE_STATE_STREAM_DESC {
                    SizeInBytes: size_of_val(&stream),
                    pPipelineStateSubobjectStream: (&raw mut stream).cast(),
                })
                .unwrap();
            let argument = D3D12_INDIRECT_ARGUMENT_DESC {
                Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_MESH,
                ..Default::default()
            };
            let mut signature = None;
            device
                .CreateCommandSignature(
                    &D3D12_COMMAND_SIGNATURE_DESC {
                        ByteStride: 12,
                        NumArgumentDescs: 1,
                        pArgumentDescs: &argument,
                        NodeMask: 0,
                    },
                    None,
                    &mut signature,
                )
                .unwrap();
            Self(root, pipeline, signature.unwrap())
        }
    }

    fn record(
        &self,
        list: &HostList,
        host: &Host,
        args: &ID3D12Resource,
    ) -> (
        ID3D12Resource,
        (ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT),
    ) {
        let target = native_resource(
            host,
            D3D12_HEAP_TYPE_DEFAULT,
            &texture_desc(
                16,
                16,
                DXGI_FORMAT_R8G8B8A8_UNORM,
                D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ),
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            Some(&D3D12_CLEAR_VALUE {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                Anonymous: D3D12_CLEAR_VALUE_0 {
                    Color: [0., 0., 0., 1.],
                },
            }),
        );
        let rtv = host.rtv(&target, None);
        // SAFETY: a live target, signature with one DispatchMesh argument and an open list.
        unsafe {
            list.list
                .ClearRenderTargetView(rtv, &[0., 0., 0., 1.], None);
            list.list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.list.RSSetViewports(&[D3D12_VIEWPORT {
                Width: 16.,
                Height: 16.,
                MaxDepth: 1.,
                ..Default::default()
            }]);
            list.list.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: 16,
                bottom: 16,
            }]);
            list.list.SetGraphicsRootSignature(&self.0);
            list.list.SetPipelineState(&self.1);
            list.list.ExecuteIndirect(&self.2, 1, args, 0, None, 0);
            list.list.ResourceBarrier(&[transition(
                &target,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
        }
        let readback = crate::capture::read_back(&host.device, &list.list, &target).unwrap();
        (target, readback)
    }
}

fn verify_indirect(
    host: &Host,
    args: &ID3D12Resource,
    mesh: Option<&Mesh>,
    visible: bool,
) -> [u32; 3] {
    let list = host.new_list().unwrap();
    let back = native_resource(
        host,
        D3D12_HEAP_TYPE_READBACK,
        &buffer_desc(12, D3D12_RESOURCE_FLAG_NONE),
        D3D12_RESOURCE_STATE_COPY_DEST,
        None,
    );
    // SAFETY: an open list, three uints in each resource, and native UAV initial state.
    unsafe {
        list.list.ResourceBarrier(&[transition(
            args,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE | D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
        )]);
        list.list.CopyBufferRegion(&back, 0, args, 0, 12);
    }
    let image = mesh.map(|pipeline| pipeline.record(&list, host, args));
    // SAFETY: the source and indirect uses are recorded before returning to UAV state.
    unsafe {
        list.list.ResourceBarrier(&[transition(
            args,
            D3D12_RESOURCE_STATE_COPY_SOURCE | D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
    }
    list.close();
    host.execute(&[&list]);
    if let Some((_target, (pixels, footprint))) = image {
        let pixels = texture_words(&pixels, footprint);
        const BLACK: u32 = u32::from_le_bytes([0, 0, 0, 255]);
        const MAGENTA: u32 = u32::from_le_bytes([255, 0, 255, 255]);
        let colored = pixels.iter().filter(|&&p| p == MAGENTA).count();
        assert!(pixels.iter().all(|&p| p == BLACK || p == MAGENTA));
        assert_eq!(pixels[8 * 16 + 8], if visible { MAGENTA } else { BLACK });
        assert_eq!(colored != 0, visible);
        eprintln!("indirect mesh: colored={colored}/{}", pixels.len());
    }
    let mut mapped = std::ptr::null_mut();
    // SAFETY: the queue completed and the readback buffer holds three words.
    unsafe {
        back.Map(0, None, Some(&mut mapped)).unwrap();
        let observed = mapped.cast::<[u32; 3]>().read();
        back.Unmap(0, Some(&D3D12_RANGE::default()));
        observed
    }
}

#[test]
#[ignore = "requires original shader compilation and a named Agility D3D12 adapter"]
fn cpu_placed_depth_drives_hiz_culling_and_mesh() {
    let (device, name) = device();
    let info = device.cast::<ID3D12InfoQueue>().unwrap();
    let supports_mesh = mesh_supported(&device);
    // SAFETY: this queue has only the optional SDK-advisory message for
    // CheckFeatureSupport, before any tested resources or work are created.
    unsafe { info.ClearStoredMessages() };
    let host = install_hiz_test_device(device);
    let mut resource_words = [0u32; 14];
    resource_words[0] = D3D12_RESOURCE_DIMENSION_TEXTURE2D.0 as u32;
    resource_words[4] = WIDTH as u32;
    resource_words[6] = HEIGHT as u32;
    resource_words[7] = 1 << 16 | 1;
    resource_words[8] = DXGI_FORMAT_R32_FLOAT.0 as u32;
    resource_words[9] = 1;
    resource_words[11] = 0x100;
    // SAFETY: these words are a complete D3D12 resource description.
    let resource_desc = unsafe {
        resource_words
            .as_ptr()
            .cast::<ResourceDesc>()
            .read_unaligned()
    };
    let layout = resource::texture_layout(&resource_desc).unwrap();
    let mut cpu = TitleMemory::new(layout.size as usize);
    let address = cpu.sampled.Value as u64;
    assert!(crate::watch::writes(address as usize, address as usize + cpu.len, false).is_none());
    let mut srv = [0u32; 16];
    srv[0] = DXGI_FORMAT_R32_FLOAT.0 as u32;
    srv[1] = D3D12_SRV_DIMENSION_TEXTURE2D.0 as u32;
    srv[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv[5] = 1;
    srv[10] = address as u32;
    srv[11] = (address >> 32) as u32;
    let heap = TitleHeap::new();
    let null = std::ptr::null();
    descriptor::create_placed_srv(null, &resource_words, &srv, heap.at(0));
    let placed = resource::texture_at(address, &resource_words).unwrap();
    let (hiz_w, hiz_h) = ((WIDTH / 2) as u32, (HEIGHT / 2) as u32);
    let placed_output = std::env::var("VICTORY_HIZ_TEST_PLACED_OUTPUT").ok();
    assert!(matches!(
        placed_output.as_deref(),
        None | Some("stale" | "updated")
    ));
    let placed_hiz = placed_output.as_ref().map(|_| {
        let mut words = resource_words;
        words[4] = hiz_w;
        words[6] = hiz_h;
        words[12] = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 as u32;
        // SAFETY: complete description words as above.
        let desc = unsafe { words.as_ptr().cast::<ResourceDesc>().read_unaligned() };
        let memory = TitleMemory::new(resource::texture_layout(&desc).unwrap().size as usize);
        let address = memory.sampled.Value as u64;
        let mut uav = [0u32; 16];
        uav[0] = DXGI_FORMAT_R32_FLOAT.0 as u32;
        uav[1] = D3D12_UAV_DIMENSION_TEXTURE2D.0 as u32;
        uav[10] = address as u32;
        uav[11] = (address >> 32) as u32;
        let texture = resource::texture_at(address, &words).unwrap();
        (memory, words, uav, texture)
    });
    let hiz = placed_hiz
        .as_ref()
        .map_or_else(|| title_texture(hiz_w, hiz_h), |hiz| hiz.3);
    descriptor::create_uav(null, hiz, null, std::ptr::null(), heap.at(1));
    descriptor::create_srv(null, hiz, std::ptr::null(), heap.at(2));
    let build = Compute::new(&host.device, "build_hiz");
    let cull = Compute::new(&host.device, "cull");
    let mesh = supports_mesh.then(|| Mesh::new(&host.device));
    if mesh.is_none() {
        eprintln!("{name}: mesh shader tier unavailable, checking compute and arguments");
    }
    let args = native_resource(
        host,
        D3D12_HEAP_TYPE_DEFAULT,
        &buffer_desc(12, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
        D3D12_RESOURCE_STATE_COMMON,
        None,
    );

    let mut seeded_hiz = None;
    for visible in [true, false] {
        let reference: Vec<u32> = (0..WIDTH * HEIGHT)
            .map(|i| {
                let x = i % WIDTH;
                let y = i / WIDTH;
                if ((x / 16 + y / 8) % 2 == 0) == visible {
                    0.75f32.to_bits()
                } else {
                    0.25f32.to_bits()
                }
            })
            .collect();
        let linear: Vec<u8> = reference
            .iter()
            .flat_map(|&word| word.to_le_bytes())
            .collect();
        layout.planes[0]
            .layout
            .tile(
                cpu.bytes(!visible),
                0,
                0,
                &linear,
                WIDTH * 4,
                WIDTH * HEIGHT * 4,
            )
            .unwrap();
        if !visible && let Some((_, words, uav, _)) = &placed_hiz {
            // The first round seeds the actual native destination. Rebind
            // only its second-round UAV via the placed descriptor path.
            descriptor::create_placed_uav(null, words, uav, heap.at(1));
        }
        let produce = host.new_list().unwrap();
        if visible {
            // SAFETY: first use of the argument buffer created in COMMON.
            unsafe {
                produce.list.ResourceBarrier(&[transition(
                    &args,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
            }
        }
        heap.prepare(&produce, &[(0, ResourceType::Srv), (1, ResourceType::Uav)]);
        build.record(
            &produce,
            heap.native(),
            &args,
            [0, 1],
            [hiz_w / 8, hiz_h / 4],
        );
        produce.close();
        host.execute(&[&produce]);
        let depth = read_texture(host, placed);
        stats("placed depth", &depth);
        assert!(
            depth == reference,
            "placed depth differs after CPU upload, alias={}",
            !visible
        );
        let pyramid = read_texture(host, hiz);
        stats("Hi-Z", &pyramid);
        let expected: Vec<_> = (0..hiz_h as usize)
            .flat_map(|y| {
                let reference = &reference;
                (0..hiz_w as usize).map(move |x| {
                    let at = 2 * (y * WIDTH + x);
                    [
                        reference[at],
                        reference[at + 1],
                        reference[at + WIDTH],
                        reference[at + WIDTH + 1],
                    ]
                    .into_iter()
                    .max()
                    .unwrap()
                })
            })
            .collect();
        if visible {
            seeded_hiz = Some(expected.clone());
        }
        let expect_old = !visible && placed_output.as_deref() == Some("stale");
        let expected = if expect_old {
            seeded_hiz.as_ref().unwrap()
        } else {
            &expected
        };
        assert!(
            &pyramid == expected,
            "Hi-Z max reduction mismatch, alias={}",
            !visible
        );
        eprintln!(
            "Hi-Z first depth={}, previous pattern={expect_old}",
            f32::from_bits(pyramid[0])
        );

        let count = host.new_list().unwrap();
        heap.prepare(&count, &[(2, ResourceType::Srv)]);
        cull.record(&count, heap.native(), &args, [2, 0], [1, 1]);
        count.close();
        host.execute(&[&count]);
        let remains_visible = visible || expect_old;
        let observed = verify_indirect(host, &args, mesh.as_ref(), remains_visible);
        assert_eq!(observed, [u32::from(remains_visible), 1, 1]);
        eprintln!("{name}: native indirect arguments {observed:?}");
        validate_queue(&info);
    }
    if placed_hiz.is_none() {
        assert_eq!(slot::<extern "system" fn(This) -> u32>(hiz, 0x10)(hiz), 0);
    }
}
