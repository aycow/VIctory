//! Original GFX10 compute fixtures for divergent lane reads and scalar
//! waterfall masks. Each fixture starts from a full wave of 64 lane IDs.

use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{Container, ShaderError, emit, io, ir, resource, structure};

const EXEC: u8 = 126;

fn sop1(op: u8, dst: u8, src: u8) -> u32 {
    0xbe80_0000 | u32::from(dst) << 16 | u32::from(op) << 8 | u32::from(src)
}

fn sop2(op: u8, dst: u8, a: u8, b: u8) -> u32 {
    0x8000_0000 | u32::from(op) << 23 | u32::from(dst) << 16 | u32::from(b) << 8 | u32::from(a)
}

fn mov32(dst: u8, src: u8) -> u32 {
    sop1(3, dst, src)
}
fn mov64(dst: u8, src: u8) -> u32 {
    sop1(4, dst, src)
}

fn valu2(op: u8, dst: u8, a: u16, b: u8) -> u32 {
    u32::from(op) << 25 | u32::from(dst) << 17 | u32::from(b) << 9 | u32::from(a)
}

fn vmov(dst: u8, src: u16) -> u32 {
    0x7e00_0200 | u32::from(dst) << 17 | u32::from(src)
}

fn vcmp(op: u8, a: u16, b: u8) -> u32 {
    0x7c00_0000 | u32::from(op) << 17 | u32::from(b) << 9 | u32::from(a)
}

fn read_lane(dst: u8, vgpr: u8, lane: u16) -> [u32; 2] {
    [
        0xd760_0000 | u32::from(dst),
        u32::from(lane) << 9 | 0x100 | u32::from(vgpr),
    ]
}

fn branch(code: u8, from: usize, to: usize) -> u32 {
    let rel = (to as isize - from as isize - 1) as i16;
    0xbf80_0000 | u32::from(code) << 16 | u32::from(rel as u16)
}

fn program(mut body: Vec<u32>) -> NativeProgram {
    // Known raw UAV in heap slot 0. Its byte address is each lane times 4.
    let mut words = vec![mov32(3, 128), mov32(2, 0), 0xf408_0101, 0xfa00_0000];
    words.append(&mut body);
    words.extend([
        valu2(0x1a, 3, 130, 0), // v_lshlrev_b32 v3, 2, v0
        0xe070_1000,
        0x8001_0203, // buffer_store_dword v2, v3, s[4:7], 0 offen
        0xbf81_0000,
    ]);
    NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 16,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [64, 1, 1],
        }),
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
    }
}

fn divergent_lane_read(source_lane: u8) -> NativeProgram {
    let mut body = vec![
        vmov(2, 135), // unselected lanes retain 7
        mov64(12, EXEC),
        vcmp(0xd4, 144, 0), // v_cmpx_gt_u32 16, v0
        branch(0x8, 3, 7),
    ];
    body.extend(read_lane(14, 0, 128 + u16::from(source_lane)));
    body.push(vmov(2, 14));
    body.push(mov64(EXEC, 12));
    program(body)
}

fn waterfall(section: bool, split_restore: bool, latch: u8) -> NativeProgram {
    let mut body = vec![
        valu2(0x1b, 1, 131, 0), // v_and_b32 v1, 3, v0, four classes
        vmov(2, 128),           // v2 result
        mov64(12, EXEC),        // initial wave mask
        mov64(16, EXEC),        // remaining lanes
    ];
    let header = body.len();
    body.extend([
        sop1(0x14, 14, 16), // s_ff1_i32_b64 s14, s[16:17]
    ]);
    body.extend(read_lane(15, 1, 14));
    body.push(vcmp(0x92, 15, 1)); // v_cmpx_eq_i32 s15, v1
    if section {
        body.extend([
            sop1(0x25, 18, 193), // s_or_saveexec_b64 s[18:19], -1
            mov32(EXEC, 255),
            0x0000_00f0, // literal lower partial mask
            mov32(EXEC + 1, 255),
            0x0000_0f00,    // literal upper partial mask
            vmov(5, 0x100), // whole-wave section's masked lane ID
        ]);
        if split_restore {
            body.extend([mov32(EXEC, 18), mov32(EXEC + 1, 19)]);
        } else {
            body.push(mov64(EXEC, 18));
        }
    } else {
        body.push(mov64(18, EXEC));
    }
    body.extend([
        valu2(0x25, 2, 145, 1), // v_add_nc_u32 v2, 17, v1
        sop2(0x15, 16, 16, 18), // s_andn2_b64 remove selected lanes
    ]);
    match latch {
        0 => body.push(mov64(EXEC, 16)),
        1 => body.extend([mov32(EXEC, 16), mov32(EXEC + 1, 17)]),
        _ => unreachable!(),
    }
    body.push(branch(0x9, body.len(), header));
    body.push(mov64(EXEC, 12));
    program(body)
}

fn emit_program(
    native: &NativeProgram,
    target: emit::Target,
) -> Result<emit::HostShader, ShaderError> {
    let p = ir::build(&native.code, native.wave64, native.vgprs)?;
    let resources = resource::track(&p, &native.layout, native.stage)?;
    let stage_io = io::raise(&p, native)?;
    let structured = structure::structure(&p)?;
    let container = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: vec![],
    };
    emit::emit(
        &container,
        native,
        &p,
        &structured,
        &stage_io,
        &resources,
        target,
    )
}

#[test]
fn wave64_culling_translation_controls_and_refusals() {
    for lane in [4, 40] {
        emit_program(&divergent_lane_read(lane), emit::Target::ANY).expect("both lane reads lower");
    }
    // These are the baseline diagnostics. A fix should replace the
    // refusal expectation with a readback comparison.
    for (label, whole_wave_section, split_restore, latch, offset) in [
        ("waterfall", false, false, 0, 0x40),
        ("partial section", true, false, 0, 0x58),
        ("word latch", true, false, 1, 0x5c),
        ("word restore", true, true, 0, 0x68),
        ("word latch and restore", true, true, 1, 0x60),
    ] {
        let error = emit_program(
            &waterfall(whole_wave_section, split_restore, latch),
            emit::Target::ANY,
        )
        .err()
        .unwrap_or_else(|| panic!("{label} translated; compare its output now"));
        assert_eq!(
            error,
            ShaderError::Unsupported {
                offset,
                what: "a lane mask holding lanes elsewhere read as data",
            },
            "{label}"
        );
    }
}

#[cfg(windows)]
mod native_tests {
    use super::*;
    use crate::emit::host::{Dispatch, Host, Root, View};
    use crate::emit::validator::Validator;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::core::{Interface, PCSTR};

    fn named_host() -> (Host, String, D3D12_FEATURE_DATA_D3D12_OPTIONS1) {
        let sdk =
            std::env::var("VICTORY_CULL_AGILITY").expect("Agility SDK 1.620.x x64 DLL directory");
        let sdk =
            std::ffi::CString::new(format!("{}\\", sdk.trim_end_matches(['/', '\\']))).unwrap();
        let vendor = std::env::var("VICTORY_CULL_ADAPTER").expect("amd, nvidia or warp");
        // SAFETY: all D3D12 descriptions and output pointers are local.
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
            let mut device = None;
            factory
                .CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)
                .unwrap();
            let host = Host {
                device: device.unwrap(),
            };
            let mut waves = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
            host.device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS1,
                    (&raw mut waves).cast(),
                    size_of_val(&waves) as u32,
                )
                .unwrap();
            (host, name, waves)
        }
    }

    fn check_debug_messages(host: &Host) {
        let queue: ID3D12InfoQueue = host.device.cast().unwrap();
        // SAFETY: retrieve each debug message into its own aligned buffer.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut bytes = 0;
                queue.GetMessage(i, None, &mut bytes).unwrap();
                let mut storage = vec![0u64; bytes.div_ceil(8)];
                let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
                queue.GetMessage(i, Some(message), &mut bytes).unwrap();
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

    #[test]
    #[ignore = "requires a named D3D12 adapter, Agility SDK 1.620.x and DXIL validator"]
    fn wave64_culling_readback() {
        let (host, name, waves) = named_host();
        let mut validator = Validator::load(Validator::default_path().as_deref()).unwrap();
        eprintln!(
            "{name} wave lane range {}..{}",
            waves.WaveLaneCountMin, waves.WaveLaneCountMax
        );
        let mut wrong = Vec::new();
        let variants = [
            ("active lane 4", divergent_lane_read(4), Some(4)),
            ("inactive lane 40", divergent_lane_read(40), Some(40)),
            ("plain waterfall", waterfall(false, false, 0), None),
            ("partial section", waterfall(true, false, 0), None),
        ];
        let target = host.target();
        for (label, native, lane) in variants {
            let shader = match emit_program(&native, target) {
                Ok(shader) => shader,
                Err(error) => {
                    eprintln!("{name}, {label}: translation refused: {error}");
                    continue;
                }
            };
            validator.validate(&mut shader.dxil.clone()).unwrap();
            let (root, pso) = host
                .compute_pipeline(&shader)
                .unwrap_or_else(|e| panic!("{name}, {label}: {e}"));
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(
                            0,
                            Root::Constants(vec![0; shader.user_data_dwords() as usize]),
                        )],
                        views: &[View::RawUav(64)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap()
                .remove(0);
            assert_eq!(out.len(), 64);
            let expected = (0..64).map(|index| match lane {
                Some(lane) => {
                    if index < 16 {
                        lane
                    } else {
                        7
                    }
                }
                None => 17 + index % 4,
            });
            let mismatches: Vec<_> = out
                .into_iter()
                .zip(expected)
                .enumerate()
                .filter_map(|(index, (got, want))| (got != want).then_some((index, got, want)))
                .collect();
            eprintln!("{name}, {label}, mismatches: {mismatches:?}");
            if !mismatches.is_empty() {
                wrong.push((label, mismatches));
            }
        }
        check_debug_messages(&host);
        assert!(
            wrong.is_empty(),
            "{name}: (source lane, [(lane, actual, expected)]): {wrong:?}"
        );
    }
}
