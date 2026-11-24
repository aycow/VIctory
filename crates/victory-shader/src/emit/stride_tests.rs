//! Original D3D12 probes for the runtime stride fallback in #193.

use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, s};

use super::{HostShader, host::Host, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const WORDS: usize = 128;
const BYTES: u64 = (WORDS * 4) as u64;

struct Pending {
    _root: ID3D12RootSignature,
    _pso: ID3D12PipelineState,
    _alloc: ID3D12CommandAllocator,
    _heap: ID3D12DescriptorHeap,
    _strides: Option<ID3D12Resource>,
    list: ID3D12CommandList,
    readback: ID3D12Resource,
}

fn shader(stride: u32, typed_input: bool) -> HostShader {
    let words = [
        0xBE85_0380u32, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0
        0xF408_0302,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[4:5], 32
        0x7E00_0200, // v_mov_b32 v0, s0; read index
        0x7E02_0201, // v_mov_b32 v1, s1; read offset
        if typed_input {
            0xE000_2000
        } else {
            0xE030_3000
        },
        0x8002_0200, // buffer_load_format_x or buffer_load_dword v2
        0x7E00_0202, // v_mov_b32 v0, s2; write index
        0x7E02_0203, // v_mov_b32 v1, s3; write offset
        0xE070_3000,
        0x8003_0200, // buffer_store_dword v2, v[0:1], s[12:15], 0
        0x7E06_028B, // v_mov_b32 v3, 11
        0xE0C8_2000,
        0x8003_0300, // buffer_atomic_add v3, v0, s[12:15], 0 idxen
        0xBF81_0000, // s_endpgm
    ];
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: [(ResourceType::Srv, 0), (ResourceType::Uav, 8)]
            .into_iter()
            .map(|(kind, offset)| UsedResource {
                kind,
                root: RootType::Table,
                table_offset_dwords: offset,
                lower_bound: 0,
                stride,
                dwords: 4,
                user_data: 4,
                slot: 1,
            })
            .collect(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
    };
    let program = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let resources = resource::track(&program, &native.layout, native.stage).unwrap();
    let stage_io = io::raise(&program, &native).unwrap();
    let structured = structure::structure(&program).unwrap();
    let out = lower::lower(&lower::Parts {
        native: &native,
        p: &program,
        st: &structured,
        io: &stage_io,
        res: &resources,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })
    .unwrap();
    let mut dxil = out.dxil;
    validator::Validator::load(validator::Validator::default_path().as_deref())
        .expect("DXIL validator")
        .validate(&mut dxil)
        .expect("translated DXIL validates");
    assert_eq!(out.stride_table, stride == 0);
    HostShader {
        stage: HwStage::Cs,
        dxil,
        user_data: out.user_data,
        samplers: out.samplers,
        static_samplers: Vec::new(),
        root_cbvs: out.root_cbvs,
        root_srvs: out.root_srvs,
        stride_table: out.stride_table,
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Raw,
    Structured(u32),
    Typed,
}

#[derive(Clone, Copy)]
struct Window {
    kind: Kind,
    start: u32,
    bytes: u32,
}

impl Window {
    fn raw(self) -> Self {
        assert_eq!(self.start % 16, 0);
        assert_eq!(self.bytes % 4, 0);
        Self {
            kind: Kind::Raw,
            ..self
        }
    }

    fn layout(self) -> (DXGI_FORMAT, u64, u32, u32) {
        let (format, stride, element) = match self.kind {
            Kind::Raw => (DXGI_FORMAT_R32_TYPELESS, 0, 4),
            Kind::Structured(stride) => (DXGI_FORMAT_UNKNOWN, stride, stride),
            Kind::Typed => (DXGI_FORMAT_R32G32B32A32_FLOAT, 0, 16),
        };
        assert!(u64::from(self.start + self.bytes) <= BYTES);
        assert_eq!(self.start % element, 0);
        assert_eq!(self.bytes % element, 0);
        (
            format,
            u64::from(self.start / element),
            self.bytes / element,
            stride,
        )
    }
}

struct Probe {
    host: Host,
    queue: ID3D12CommandQueue,
    done: ID3D12Fence,
    next: u64,
    upload: ID3D12Resource,
    zero: ID3D12Resource,
    target: ID3D12Resource,
    samplers: ID3D12DescriptorHeap,
    info: Option<ID3D12InfoQueue>,
    words: [u32; WORDS],
}

impl Probe {
    fn factory(debug: bool) -> ID3D12DeviceFactory {
        // SAFETY: SDK factory configuration happens before creating test devices.
        unsafe {
            let mut settings: Option<ID3D12SDKConfiguration1> = None;
            D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut settings).unwrap();
            let factory: ID3D12DeviceFactory = settings
                .unwrap()
                .CreateDeviceFactory(620, s!(".\\D3D12\\"))
                .expect("Agility 1.620.2 under D3D12 next to the executable");
            if debug {
                let layer: ID3D12Debug1 = factory
                    .GetConfigurationInterface(&CLSID_D3D12Debug)
                    .unwrap();
                layer.EnableDebugLayer();
                layer.SetEnableGPUBasedValidation(true);
            }
            factory
        }
    }

    fn device(factory: &ID3D12DeviceFactory, vendor: Option<u32>, debug: bool) -> Self {
        // SAFETY: DXGI descriptors and device out parameters are live.
        unsafe {
            let dxgi: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
            let adapter: IDXGIAdapter1 = match vendor {
                Some(vendor) => (0..)
                    .map_while(|index| dxgi.EnumAdapters1(index).ok())
                    .find(|a| a.GetDesc1().unwrap().VendorId == vendor)
                    .expect("requested hardware adapter"),
                None => dxgi.EnumWarpAdapter().unwrap(),
            };
            let desc = adapter.GetDesc1().unwrap();
            assert_eq!(
                desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
                vendor.is_none()
            );
            let name = String::from_utf16_lossy(&desc.Description);
            eprintln!(
                "adapter={} debug={debug} gbv={debug}",
                name.trim_end_matches('\0')
            );
            let mut device: Option<ID3D12Device> = None;
            factory
                .CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)
                .unwrap();
            let device = device.unwrap();
            let info = debug.then(|| device.cast::<ID3D12InfoQueue>().unwrap());
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let done = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let samplers = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .unwrap();
            let upload = buffer(
                &device,
                D3D12_HEAP_TYPE_UPLOAD,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                BYTES,
            );
            let zero = buffer(
                &device,
                D3D12_HEAP_TYPE_UPLOAD,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                BYTES,
            );
            let target = buffer(
                &device,
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_RESOURCE_STATE_COMMON,
                BYTES,
            );
            let words = std::array::from_fn(|i| 0x3f00_0000 + (i as u32 + 1) * 0x1000);
            fill(&upload, &words);
            fill(&zero, &[0; WORDS]);
            Self {
                host: Host { device },
                queue,
                done,
                next: 1,
                upload,
                zero,
                target,
                samplers,
                info,
                words,
            }
        }
    }

    fn view_heap(
        &self,
        src: Window,
        dst: Window,
        raw_pair: Option<(Window, Window)>,
    ) -> ID3D12DescriptorHeap {
        let device = &self.host.device;
        // SAFETY: heap slots and resources live for the entire submission.
        unsafe {
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 4,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .unwrap();
            let descriptor_size =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            let mut at = heap.GetCPUDescriptorHandleForHeapStart();
            for (src, dst) in [(src, dst), raw_pair.unwrap_or((src, dst))] {
                let (format, first, count, stride) = src.layout();
                device.CreateShaderResourceView(
                    &self.upload,
                    Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                            Buffer: D3D12_BUFFER_SRV {
                                FirstElement: first,
                                NumElements: count,
                                StructureByteStride: stride,
                                Flags: if matches!(src.kind, Kind::Raw) {
                                    D3D12_BUFFER_SRV_FLAG_RAW
                                } else {
                                    D3D12_BUFFER_SRV_FLAG_NONE
                                },
                            },
                        },
                    }),
                    at,
                );
                at.ptr += descriptor_size as usize;
                let (format, first, count, stride) = dst.layout();
                device.CreateUnorderedAccessView(
                    &self.target,
                    None,
                    Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                            Buffer: D3D12_BUFFER_UAV {
                                FirstElement: first,
                                NumElements: count,
                                StructureByteStride: stride,
                                CounterOffsetInBytes: 0,
                                Flags: if matches!(dst.kind, Kind::Raw) {
                                    D3D12_BUFFER_UAV_FLAG_RAW
                                } else {
                                    D3D12_BUFFER_UAV_FLAG_NONE
                                },
                            },
                        },
                    }),
                    at,
                );
                at.ptr += descriptor_size as usize;
            }
            heap
        }
    }

    fn stride_data(&self, src: u32, dst: u32) -> ID3D12Resource {
        let table = buffer(
            &self.host.device,
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            16,
        );
        fill(&table, &[src, dst, src, dst]);
        table
    }

    fn record(
        &self,
        shader: &HostShader,
        heap: &ID3D12DescriptorHeap,
        stride_table: Option<&ID3D12Resource>,
        alias: bool,
        args: [u32; 4],
    ) -> Pending {
        assert_eq!(shader.stride_table, stride_table.is_some());
        assert!(shader.root_srvs.is_empty() && shader.root_cbvs.is_empty());
        let device = &self.host.device;
        let (root, pso) = self
            .host
            .compute_pipeline(shader)
            .expect("translated pipeline");
        let readback = buffer(
            device,
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_STATE_COPY_DEST,
            BYTES,
        );
        let mut constants = vec![0; shader.user_data_dwords() as usize];
        for u in &shader.user_data {
            constants[u.index as usize] = match (u.root, u.slot) {
                (RootType::Constants, 0) => args[u.dword as usize],
                (RootType::Table, 1) => {
                    if alias {
                        64
                    } else {
                        0
                    }
                }
                other => panic!("unexpected root user data {other:?}"),
            };
        }
        // SAFETY: the list owns an allocator and all referenced objects survive retirement.
        unsafe {
            let alloc: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso)
                .unwrap();
            list.ResourceBarrier(&[transition(
                &self.target,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
            list.CopyBufferRegion(&self.target, 0, &self.zero, 0, BYTES);
            list.ResourceBarrier(&[transition(
                &self.target,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
            list.SetDescriptorHeaps(&[Some(heap.clone()), Some(self.samplers.clone())]);
            list.SetComputeRootSignature(&root);
            list.SetComputeRoot32BitConstants(
                0,
                constants.len() as u32,
                constants.as_ptr().cast(),
                0,
            );
            if let Some(table) = stride_table {
                list.SetComputeRootShaderResourceView(1, table.GetGPUVirtualAddress());
            }
            list.Dispatch(1, 1, 1);
            list.ResourceBarrier(&[transition(
                &self.target,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.CopyBufferRegion(&readback, 0, &self.target, 0, BYTES);
            list.ResourceBarrier(&[transition(
                &self.target,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
            list.Close().unwrap();
            Pending {
                _root: root,
                _pso: pso,
                _alloc: alloc,
                _heap: heap.clone(),
                _strides: stride_table.cloned(),
                list: list.cast().unwrap(),
                readback,
            }
        }
    }

    fn submit(&mut self, pending: &[&Pending]) -> u64 {
        let lists: Vec<_> = pending.iter().map(|p| Some(p.list.clone())).collect();
        let value = self.next;
        self.next += 1;
        // SAFETY: all pending dispatch owners live through the caller's wait.
        unsafe {
            self.queue.ExecuteCommandLists(&lists);
            self.queue.Signal(&self.done, value).unwrap();
        }
        value
    }

    fn wait(&self, value: u64) {
        let deadline = Instant::now() + Duration::from_secs(20);
        // SAFETY: the fence and queue's device stay live while polling.
        unsafe {
            while self.done.GetCompletedValue() < value {
                self.host.device.GetDeviceRemovedReason().unwrap();
                assert!(Instant::now() < deadline, "dispatch fence timed out");
                std::thread::sleep(Duration::from_millis(1));
            }
            self.host.device.GetDeviceRemovedReason().unwrap();
        }
    }

    fn submit_read(&mut self, pending: &Pending) -> [u32; WORDS] {
        let fence = self.submit(&[pending]);
        self.wait(fence);
        self.check_debug();
        Self::read(pending)
    }

    fn read(pending: &Pending) -> [u32; WORDS] {
        // SAFETY: the caller waits for the fence before accessing the full readback.
        unsafe {
            let mut at = std::ptr::null_mut();
            pending.readback.Map(0, None, Some(&mut at)).unwrap();
            let words = *at.cast::<[u32; WORDS]>();
            pending.readback.Unmap(0, None);
            words
        }
    }

    fn check_debug(&self) {
        let Some(info) = &self.info else { return };
        // SAFETY: message buffers satisfy the expected byte length and alignment.
        unsafe {
            for i in 0..info.GetNumStoredMessages() {
                let mut bytes = 0;
                info.GetMessage(i, None, &mut bytes).unwrap();
                let mut buf = vec![0u64; bytes.div_ceil(8)];
                let msg = buf.as_mut_ptr().cast::<D3D12_MESSAGE>();
                info.GetMessage(i, Some(msg), &mut bytes).unwrap();
                let msg = &*msg;
                let text = std::slice::from_raw_parts(msg.pDescription, msg.DescriptionByteLength);
                assert!(
                    msg.Severity.0 > D3D12_MESSAGE_SEVERITY_ERROR.0,
                    "debug layer {}: {}",
                    msg.ID.0,
                    String::from_utf8_lossy(text)
                );
            }
            assert_eq!(info.GetNumMessagesDiscardedByMessageCountLimit(), 0);
            info.ClearStoredMessages();
        }
    }
}

fn reference(
    words: &[u32; WORDS],
    src: Window,
    dst: Window,
    src_stride: u32,
    dst_stride: u32,
    args: [u32; 4],
) -> [u32; WORDS] {
    let [i, offset, j, out_offset] = args;
    let mut expected = [0; WORDS];
    let input = ((src.start + i * src_stride + offset) / 4) as usize;
    let output = ((dst.start + j * dst_stride + out_offset) / 4) as usize;
    let atomic = ((dst.start + j * dst_stride) / 4) as usize;
    expected[output] = words[input];
    expected[atomic] += 11;
    expected
}

fn structured_windows(stride: u32) -> (Window, Window) {
    let aligned = (16..)
        .step_by(16)
        .find(|start| start % stride == 0)
        .unwrap();
    let src = Window {
        kind: Kind::Structured(stride),
        start: aligned,
        bytes: stride * 4,
    };
    (
        src,
        Window {
            start: aligned * 2,
            ..src
        },
    )
}

#[test]
#[ignore = "requires D3D12 Agility 1.620.2 and the AMD/NVIDIA/WARP adapters"]
fn structured_stride_alias_matrix() {
    let factory = Probe::factory(true);
    let fallback = shader(0, false);
    let typed = shader(16, true);
    let strides = [8, 12, 16, 20, 32].map(|s| (s, shader(s, false)));
    for vendor in [Some(0x1002), Some(0x10de), None] {
        let mut probe = Probe::device(&factory, vendor, true);
        for (stride, honest) in &strides {
            let (src, dst) = structured_windows(*stride);
            let heap = probe.view_heap(src, dst, Some((src.raw(), dst.raw())));
            let table = probe.stride_data(*stride, *stride);
            let args = [2, stride - 4, 1, stride - 4];
            let expected = reference(&probe.words, src, dst, *stride, *stride, args);
            let structured = probe.record(honest, &heap, None, false, args);
            assert_eq!(
                probe.submit_read(&structured),
                expected,
                "structured stride {stride}"
            );
            let alias = probe.record(&fallback, &heap, Some(&table), true, args);
            assert_eq!(
                probe.submit_read(&alias),
                expected,
                "raw alias stride {stride}"
            );
            eprintln!(
                "stride={stride} src_start={} dst_start={} honest_and_alias=pass",
                src.start, dst.start
            );
        }
        let (src, dst) = structured_windows(16);
        let src = Window {
            kind: Kind::Typed,
            ..src
        };
        let heap = probe.view_heap(src, dst, Some((src.raw(), dst.raw())));
        let table = probe.stride_data(16, 16);
        let args = [2, 0, 1, 4];
        let expected = reference(&probe.words, src, dst, 16, 16, args);
        let honest = probe.record(&typed, &heap, None, false, args);
        assert_eq!(probe.submit_read(&honest), expected, "typed SRV");
        let alias = probe.record(&fallback, &heap, Some(&table), true, args);
        assert_eq!(
            probe.submit_read(&alias),
            expected,
            "raw alias of typed SRV"
        );
        eprintln!("typed=pass typed_alias=pass");

        let (src, _) = structured_windows(20);
        let (_, dst) = structured_windows(8);
        let heap = probe.view_heap(src, dst, Some((src.raw(), dst.raw())));
        let table = probe.stride_data(20, 8);
        let args = [2, 16, 1, 4];
        let alias = probe.record(&fallback, &heap, Some(&table), true, args);
        assert_eq!(
            probe.submit_read(&alias),
            reference(&probe.words, src, dst, 20, 8, args),
            "source and destination must use distinct slot metadata"
        );
        eprintln!("mixed_src20_dst8=pass");
    }
}

#[test]
#[ignore = "requires D3D12 Agility 1.620.2 and the AMD/NVIDIA/WARP adapters"]
fn structured_stride_fence_lifetime() {
    let factory = Probe::factory(true);
    let fallback = shader(0, false);
    let args = [2, 4, 1, 4];
    for vendor in [Some(0x1002), Some(0x10de), None] {
        let mut probe = Probe::device(&factory, vendor, true);
        let (old_src, old_dst) = structured_windows(12);
        let (new_src, new_dst) = structured_windows(20);
        let old_heap = probe.view_heap(old_src, old_dst, Some((old_src.raw(), old_dst.raw())));
        let new_heap = probe.view_heap(new_src, new_dst, Some((new_src.raw(), new_dst.raw())));
        let old_strides = probe.stride_data(12, 12);
        let new_strides = probe.stride_data(20, 20);
        let old = probe.record(&fallback, &old_heap, Some(&old_strides), true, args);
        let new = probe.record(&fallback, &new_heap, Some(&new_strides), true, args);
        let gate: ID3D12Fence;
        // SAFETY: the gate lives until all queued lists complete. Pending owns each heap
        // and stride revision even after the staging-side references are dropped.
        unsafe {
            gate = probe
                .host
                .device
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
                .unwrap();
            probe.queue.Wait(&gate, 1).unwrap();
            let done = probe.submit(&[&old, &new]);
            drop(old_heap);
            drop(new_heap);
            drop(old_strides);
            drop(new_strides);
            gate.Signal(1).unwrap();
            probe.wait(done);
        }
        probe.check_debug();
        assert_eq!(
            Probe::read(&old),
            reference(&probe.words, old_src, old_dst, 12, 12, args)
        );
        assert_eq!(
            Probe::read(&new),
            reference(&probe.words, new_src, new_dst, 20, 20, args)
        );

        let shared_heap = probe.view_heap(old_src, old_dst, Some((old_src.raw(), old_dst.raw())));
        let shared_strides = probe.stride_data(12, 12);
        let pending = probe.record(&fallback, &shared_heap, Some(&shared_strides), true, args);
        // SAFETY: changing only the data before the GPU can execute demonstrates a
        // shared mapped stride table's generation race without overwriting descriptors.
        unsafe {
            probe.queue.Wait(&gate, 2).unwrap();
            let done = probe.submit(&[&pending]);
            fill(&shared_strides, &[20, 20, 20, 20]);
            gate.Signal(2).unwrap();
            probe.wait(done);
        }
        probe.check_debug();
        assert_eq!(
            Probe::read(&pending),
            reference(&probe.words, old_src, old_dst, 20, 20, args)
        );
        assert_ne!(Probe::read(&pending), Probe::read(&old));
        eprintln!("old_generation=pass new_generation=pass shared_stride_overwrite=observed");
    }
}

#[test]
#[ignore = "requires D3D12 Agility 1.620.2 and the AMD/NVIDIA/WARP adapters"]
fn structured_stride_bounds() {
    let factory = Probe::factory(true);
    let fallback = shader(0, false);
    for vendor in [Some(0x1002), Some(0x10de), None] {
        let mut probe = Probe::device(&factory, vendor, true);
        let (src, dst) = structured_windows(12);
        let heap = probe.view_heap(src, dst, Some((src.raw(), dst.raw())));
        let table = probe.stride_data(12, 12);
        let zero_stride = probe.stride_data(0, 12);
        let between_records = [1, 12, 1, 4];
        let pending = probe.record(&fallback, &heap, Some(&table), true, between_records);
        assert_eq!(
            probe.submit_read(&pending),
            reference(&probe.words, src, dst, 12, 12, between_records),
            "the current flattened path crosses into the next record"
        );
        let zero_metadata = [2, 4, 1, 4];
        let pending = probe.record(&fallback, &heap, Some(&zero_stride), true, zero_metadata);
        assert_eq!(
            probe.submit_read(&pending),
            reference(&probe.words, src, dst, 0, 12, zero_metadata),
            "a zero stride reads element zero even when the shader requested element two"
        );
        let beyond_view = [4, 0, 1, 4];
        let pending = probe.record(&fallback, &heap, Some(&table), true, beyond_view);
        let mut expected = [0; WORDS];
        expected[((dst.start + 12) / 4) as usize] = 11;
        assert_eq!(
            probe.submit_read(&pending),
            expected,
            "view-bounded raw alias must not read data outside the source descriptor"
        );
        eprintln!("raw_cross_record=observed zero_stride_alias=observed descriptor_extent=pass");
    }
}

#[test]
#[ignore = "explicitly exercises invalid cross-kind descriptor bindings with validation disabled"]
fn structured_stride_incompatible_descriptor_characterization() {
    // The contradictory handle/descriptor bindings below have undefined results.
    // Run this opt-in characterization separately from the validation suite.
    let factory = Probe::factory(false);
    let fallback = shader(0, false);
    for vendor in [Some(0x1002), Some(0x10de), None] {
        let mut probe = Probe::device(&factory, vendor, false);
        let (src, dst) = structured_windows(12);
        let (typed_src, _) = structured_windows(16);
        let typed_src = Window {
            kind: Kind::Typed,
            ..typed_src
        };
        for (name, src, src_stride, dst) in [
            ("raw_handle_structured_srv", src, 12, dst.raw()),
            ("raw_handle_typed_srv", typed_src, 16, dst.raw()),
            ("raw_handle_structured_uav", src.raw(), 12, dst),
        ] {
            let heap = probe.view_heap(src, dst, None);
            let table = probe.stride_data(src_stride, 12);
            let args = [1, 4, 1, 4];
            let pending = probe.record(&fallback, &heap, Some(&table), false, args);
            let actual = probe.submit_read(&pending);
            let expected = reference(&probe.words, src, dst, src_stride, 12, args);
            let written: Vec<_> = actual
                .iter()
                .enumerate()
                .filter(|(_, value)| **value != 0)
                .map(|(index, value)| (index * 4, *value))
                .collect();
            let stored = actual[((dst.start + 16) / 4) as usize];
            let atomic = actual[((dst.start + 12) / 4) as usize];
            eprintln!(
                "{name} equals_reference={} stored={stored:#x} atomic={atomic} nonzero_byte_offsets={written:?}",
                actual == expected
            );
        }
        let heap = probe.view_heap(src.raw(), dst.raw(), None);
        let table = probe.stride_data(6, 12);
        let args = [1, 4, 1, 4];
        let pending = probe.record(&fallback, &heap, Some(&table), false, args);
        let actual = probe.submit_read(&pending);
        let stored = actual[((dst.start + 16) / 4) as usize];
        let dword_aligned = probe.words[((src.start + 8) / 4) as usize];
        let next = probe.words[((src.start + 12) / 4) as usize];
        let cross_dword = (dword_aligned >> 16) | (next << 16);
        eprintln!(
            "stride6_byte10 observed={stored:#x} dword_aligned={dword_aligned:#x} unaligned={cross_dword:#x}"
        );
    }
}

fn buffer(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
    bytes: u64,
) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: if heap == D3D12_HEAP_TYPE_DEFAULT {
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
        } else {
            D3D12_RESOURCE_FLAG_NONE
        },
        ..Default::default()
    };
    let mut result: Option<ID3D12Resource> = None;
    // SAFETY: the description and output parameter are live.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: heap,
                    CreationNodeMask: 1,
                    VisibleNodeMask: 1,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                state,
                None,
                &mut result,
            )
            .unwrap();
    }
    result.unwrap()
}

fn fill(upload: &ID3D12Resource, words: &[u32]) {
    // SAFETY: the caller supplies an upload buffer with space for all words.
    unsafe {
        let mut at = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut at)).unwrap();
        std::ptr::copy_nonoverlapping(words.as_ptr(), at.cast::<u32>(), words.len());
        upload.Unmap(0, None);
    }
}

fn transition(
    res: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: the API borrows the resource during barrier recording only.
                pResource: unsafe { std::mem::transmute_copy(res) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}
