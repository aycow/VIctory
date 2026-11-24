use super::*;
use crate::host::{
    address_map_test_dispatch_us, address_map_test_prepare_us, install_address_map_test_device,
};
use crate::pipeline::{address_map_test_shader, host::HostPipeline};
use victory_shader::native::{ComputeIo, StageIo};
use victory_shader::{
    Container, HwStage, NativeProgram, RootParam, RootType, emit, io, ir, resource, structure,
};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCSTR};

fn copy_program() -> emit::HostShader {
    // One lane. The scalar and vector reads use a guest address in s[0:1].
    // Scalar data goes to output +16 and also increments output +20.
    let words = [
        0xDC38_8000u32,
        0x0200_0000, // global_load_dwordx4 v[2:5], v0, s[0:1]
        0xF400_0300,
        0xFA00_0000, // s_load_dword s12, s[0:1], 0
        0xBF8C_0070, // s_waitcnt vmcnt(0) lgkmcnt(0)
        0x7E0C_020C, // v_mov_b32 v6, s12
        0xDC78_8000,
        0x0002_0200, // global_store_dwordx4 v0, v[2:5], s[2:3]
        0xDC70_8010,
        0x0002_0600, // global_store_dword v0, v6, s[2:3] offset:16
        0xDCC8_8014,
        0x0002_0600, // global_atomic_add v0, v6, s[2:3] offset:20
        0xBF81_0000,
    ];
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Constants,
            table_offset_dwords: 0,
            sgprs: 4,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: Vec::new(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 4 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, false, 8).unwrap();
    let flow = structure::structure(&p).unwrap();
    let uses = resource::track(&p, &native.layout, native.stage, Some(&flow)).unwrap();
    let stage = io::raise(&p, &native).unwrap();
    let container = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let mut shader = emit::emit(&container, &native, &p, &flow, &stage, &uses).unwrap();
    assert!(shader.address_map);
    emit::validator::Validator::load(emit::validator::Validator::default_path().as_deref())
        .expect("DXIL validator")
        .validate(&mut shader.dxil)
        .unwrap();
    shader
}

pub(crate) fn device(validate: bool) -> (ID3D12Device, String) {
    let sdk = std::env::var("VICTORY_TABLE_TEST_AGILITY").expect("Agility SDK 1.620.x x64 path");
    let sdk = std::ffi::CString::new(format!("{}\\", sdk.trim_end_matches(['/', '\\']))).unwrap();
    let vendor = std::env::var("VICTORY_TABLE_TEST_ADAPTER").expect("amd, nvidia or warp");
    // SAFETY: the configuration, adapter enumeration and output pointers are ours.
    unsafe {
        let mut configuration: Option<ID3D12SDKConfiguration1> = None;
        D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut configuration).unwrap();
        let factory: ID3D12DeviceFactory = configuration
            .unwrap()
            .CreateDeviceFactory(620, PCSTR(sdk.as_ptr().cast()))
            .unwrap();
        if validate {
            let debug: ID3D12Debug = factory
                .GetConfigurationInterface(&CLSID_D3D12Debug)
                .unwrap();
            debug.EnableDebugLayer();
            debug
                .cast::<ID3D12Debug1>()
                .unwrap()
                .SetEnableGPUBasedValidation(true);
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
        let mut device: Option<ID3D12Device> = None;
        factory
            .CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)
            .unwrap();
        let device = device.unwrap();
        let configuration: ID3D12DeviceConfiguration = device.cast().unwrap();
        let configuration = configuration.GetDesc();
        assert_eq!(configuration.SDKVersion, 620);
        assert_eq!(
            configuration.Flags.0 & D3D12_DEVICE_FLAG_GPU_BASED_VALIDATION_ENABLED.0 != 0,
            validate
        );
        assert_eq!(configuration.GpuBasedValidationFlags, 0);
        (device, name)
    }
}

#[test]
#[ignore = "requires a chosen D3D12 adapter, Agility 1.620.x and the DXIL validator"]
fn computed_map_supports_shared_and_shadow_backing() {
    let bench = std::env::var_os("VICTORY_MAP_TEST_BENCH").is_some();
    let overlap = std::env::var_os("VICTORY_MAP_TEST_OVERLAP").is_some();
    let (device, name) = device(!bench);
    let messages = (!bench).then(|| device.cast::<ID3D12InfoQueue>().unwrap());
    let host = install_address_map_test_device(device);
    let shader = address_map_test_shader(copy_program());
    let pipeline = HostPipeline::new(&[shader], None).expect("translated map pipeline");
    let stage = &pipeline.stages[0];
    let heap = HostHeap::new(false, 1).unwrap();
    let samplers = HostHeap::new(true, 1).unwrap();

    let title = Box::new([7u32, 11, 19, 23]);
    let guest = title.as_ptr() as u64;
    crate::shadow::register(guest as usize, guest, 16);
    let shadow = crate::shadow::find(guest, 16).unwrap();
    let (first, first_cpu) = host.shared_buffer(16).unwrap();
    let (replacement, replacement_cpu) = host.shared_buffer(16).unwrap();
    assert_ne!(first_cpu as u64, guest);
    // SAFETY: the test owns both mapped UAV buffers; no GPU work has run.
    unsafe {
        first_cpu.cast::<[u32; 4]>().write([31, 37, 41, 43]);
        replacement_cpu.cast::<[u32; 4]>().write([47, 53, 59, 61]);
    }
    let mut inputs = vec![
        Backing {
            span: (guest, 16),
            resource: &shadow.buffer,
            shadow: Some(&shadow),
        },
        Backing {
            span: (guest, 16),
            resource: &first,
            shadow: None,
        },
        Backing {
            span: (guest, 16),
            resource: &replacement,
            shadow: None,
        },
        Backing {
            span: (guest, 16),
            resource: &replacement,
            shadow: None,
        },
    ];
    if overlap {
        inputs.push(Backing {
            span: (guest, 16),
            resource: &first,
            shadow: None,
        });
    }
    const GUEST_OUT: u64 = 0x20_ffff_fffc;
    let mut runs = Vec::new();
    for (i, input) in inputs.into_iter().enumerate() {
        let list = host.new_list().unwrap();
        let (output, output_cpu) = host.shared_buffer(32).unwrap();
        let mut backings = vec![
            input,
            Backing {
                span: (GUEST_OUT, 32),
                resource: &output,
                shadow: None,
            },
        ];
        if i == 4 {
            // This inner mapping owns the last three guest dwords, while
            // the outer raw view is still wide enough to read all four.
            backings.push(Backing {
                span: (guest + 4, 12),
                resource: &replacement,
                shadow: None,
            });
        }
        let (map, slots) = make_backed(&list, &heap, &backings).unwrap();
        let input_addr = if i == 3 { 0x40_ffff_0000 } else { guest };
        let title_words = [
            input_addr as u32,
            (input_addr >> 32) as u32,
            GUEST_OUT as u32,
            (GUEST_OUT >> 32) as u32,
        ];
        let (param, count) = stage.user_data.unwrap();
        let mut values = vec![0u32; count as usize];
        for u in &stage.translated().host.user_data {
            values[u.index as usize] = title_words[u.dword as usize];
        }
        // SAFETY: root resources and the two descriptor heaps outlive this submission.
        unsafe {
            output_cpu.cast::<[u32; 8]>().write([0xdeca_fbad; 8]);
            list.list
                .SetDescriptorHeaps(&[Some(heap.heap.clone()), Some(samplers.heap.clone())]);
            list.list.SetComputeRootSignature(&pipeline.root);
            list.list.SetPipelineState(&pipeline.pso);
            list.list
                .SetComputeRoot32BitConstants(param, count, values.as_ptr().cast(), 0);
            list.list
                .SetComputeRootShaderResourceView(stage.address_map.unwrap(), map);
            if bench {
                list.note_op(|| "address-map dispatch".to_owned());
            }
            list.list.Dispatch(1, 1, 1);
        }
        list.close();
        runs.push((list, slots, output_cpu));
    }
    // Drop map inputs before execution. The native recordings retain them.
    drop((first, replacement));
    let lists: Vec<_> = runs.iter().map(|run| &run.0).collect();
    let start = std::time::Instant::now();
    host.execute(&lists);
    let wall = start.elapsed();
    let dispatches = if bench {
        benchmark_replay(host, [&runs[0].0, &runs[1].0]);
        [32, 32, 1, 1, 1]
    } else {
        [1; 5]
    };
    let expected = [
        [7, 11, 19, 23, 7],
        [31, 37, 41, 43, 31],
        [47, 53, 59, 61, 47],
        [0, 0, 0, 0, 0],
        [31, 47, 53, 59, 31],
    ];
    for (i, (list, (first, count), data)) in runs.into_iter().enumerate() {
        // SAFETY: the queue completed and this list still owns the mapped output.
        let words = unsafe { data.cast::<[u32; 8]>().read() };
        assert_eq!(words[..5], expected[i], "case {i}");
        assert_eq!(words[5], 0xdeca_fbad + dispatches[i] * expected[i][4]);
        assert_eq!(words[6..], [0xdeca_fbad; 2]);
        heap.release(first, count);
        drop(list);
    }
    if let Some(messages) = messages {
        check_debug_messages(&messages);
    }
    crate::shadow::unregister(guest as usize, guest);
    eprintln!(
        "{name}: computed scalar/vector reads, stores, atomics and miss correct; four-list wall {wall:?}"
    );
}

fn benchmark_replay(host: &crate::host::Host, lists: [&HostList; 2]) {
    const WARMUPS: usize = 10;
    const SAMPLES: usize = 21;
    let mut stats = [[[0.0; SAMPLES]; 3]; 2];
    for trial in 0..WARMUPS + SAMPLES {
        // Alternate order so neither backing always sees the first submit.
        for i in [trial % 2, 1 - trial % 2] {
            address_map_test_prepare_us();
            let began = std::time::Instant::now();
            host.execute(&[lists[i]]);
            let wall = began.elapsed().as_secs_f64() * 1_000_000.0;
            let prepare = address_map_test_prepare_us() as f64;
            let gpu = address_map_test_dispatch_us(lists[i]);
            if trial >= WARMUPS {
                let sample = trial - WARMUPS;
                stats[i][0][sample] = prepare;
                stats[i][1][sample] = gpu;
                stats[i][2][sample] = wall;
            }
        }
    }
    for (backing, metrics) in ["shadow", "shared"].into_iter().zip(stats) {
        let [prepare, gpu, wall] = metrics.map(|mut samples| {
            samples.sort_by(f64::total_cmp);
            samples[SAMPLES / 2]
        });
        eprintln!(
            "{backing}: median CPU preparation {prepare:.1} us, GPU span {gpu:.2} us, synchronous submit {wall:.2} us"
        );
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
