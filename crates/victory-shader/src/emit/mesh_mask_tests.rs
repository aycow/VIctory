//! Numerical Wave32 ballots in mesh shaders without a fixed host wave size.

use super::container::SigElement;
use super::{HostShader, emit};
use crate::native::{HwStage, MeshIo, NativeProgram, StageIo};
use crate::{Container, FourCC, ShaderError, io, ir, resource, structure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ballot {
    None,
    Entry,
    Divergent,
}

fn mesh(
    threads: u32,
    ballot: Ballot,
    wave64: bool,
    lds_bytes: u32,
) -> Result<HostShader, ShaderError> {
    let mut words = vec![
        0xD765_0000,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v0, -1, 0
        0x9004_9803, // s_lshr_b32 s4, s3, 24 (console wave in group)
    ];
    if lds_bytes != 0 {
        words.extend([
            0x8F0E_8504, // s_lshl_b32 s14, s4, 5
            0x4A08_000E, // v_add_nc_u32 v4, s14, v0
            0x3408_0882, // v_lshlrev_b32 v4, 2, v4
            0xD834_0000,
            0x0000_0004, // ds_write_b32 v4, v0 (separate from ballot scratch)
        ]);
    }
    let branch = if ballot == Ballot::Divergent {
        words.extend([
            0xBE8B_0380, // s_mov_b32 s11, 0
            0x7D88_0090, // v_cmp_gt_u32 vcc_lo, 16, v0
            0xBE8D_3C6A, // s_and_saveexec_b32 s13, vcc_lo
            0xBF88_0000, // s_cbranch_execz restore_exec
        ]);
        Some(words.len() - 1)
    } else {
        None
    };
    if ballot != Ballot::None {
        // Predicates that include sparse high/low bits and differ between
        // adjacent console waves. Compare both the bits and the popcount
        // before either wave is allowed to emit its triangle.
        words.extend([
            0xBE89_03FF,
            0x8000_0081, // s_mov_b32 s9, mask 0
            0xBE8A_03FF,
            0x4000_0182, // s_mov_b32 s10, mask 1
            0xBF06_8004, // s_cmp_eq_u32 s4, 0
            0x8505_0A09, // s_cselect_b32 s5, s9, s10
            0x7E02_0205, // v_mov_b32 v1, s5
            0x2C02_0300, // v_lshrrev_b32 v1, v0, v1
            0x3602_0281, // v_and_b32 v1, 1, v1
            0x7D84_0281, // v_cmp_eq_u32 vcc_lo, 1, v1
            0xBE86_036A, // s_mov_b32 s6, vcc_lo
            0xBE87_0F06, // s_bcnt1_i32_b32 s7, s6
            0xBF07_0506, // s_cmp_lg_u32 s6, s5
            0x850B_80FF,
            0x8000_0000, // s_cselect_b32 s11, cull, 0
            0xBE8C_0F05, // s_bcnt1_i32_b32 s12, s5
            0xBF07_0C07, // s_cmp_lg_u32 s7, s12
            0x850B_0BFF,
            0x8000_0000, // s_cselect_b32 s11, cull, s11
            0xBE89_0705, // s_not_b32 s9, s5
            0x7D84_0280, // v_cmp_eq_u32 vcc_lo, 0, v1
            0xBF07_096A, // s_cmp_lg_u32 vcc_lo, s9
            0x850B_0BFF,
            0x8000_0000, // s_cselect_b32 s11, cull, s11
        ]);
    } else {
        words.push(0xBE8B_0380); // s_mov_b32 s11, 0
    }
    if let Some(branch) = branch {
        words[branch] |= (words.len() - branch - 1) as u32;
        words.push(0x887E_0D7E); // s_or_b32 exec_lo, exec_lo, s13
    }
    words.extend([
        0x7D84_0081, // v_cmp_eq_u32 vcc_lo, 1, v0
        0x7E1E_0280, // v_mov_b32 v15, 0
        0x0214_1EC1, // v_cndmask_b32 v10, -1, v15, vcc_lo
        0x4A14_1404, // v_add_nc_u32 v10, s4, v10
        0x7E14_0B0A, // v_cvt_f32_i32 v10, v10
        0x7D84_0082, // v_cmp_eq_u32 vcc_lo, 2, v0
        0x7E1E_02F2, // v_mov_b32 v15, 1.0
        0x0216_1EF3, // v_cndmask_b32 v11, -1.0, v15, vcc_lo
        0x7E18_02F0, // v_mov_b32 v12, 0.5
        0x7E1A_02F2, // v_mov_b32 v13, 1.0
        0xF800_00CF,
        0x0D0C_0B0A, // exp pos0 v10, v11, v12, v13
        0x9308_FF04,
        0x0200_8020, // s_mul_i32 s8, s4, 32 packed in three indices
        0x8808_FF08,
        0x0020_0400, // s_or_b32 s8, s8, packed (0, 1, 2)
        0x8808_0B08, // s_or_b32 s8, s8, s11
        0x7E1C_0208, // v_mov_b32 v14, s8
        0x7D84_0080, // v_cmp_eq_u32 vcc_lo, 0, v0
        0xBE8D_3C6A, // s_and_saveexec_b32 s13, vcc_lo
        0xF800_0941,
        0x0000_000E, // exp prim v14, off, off, off done
        0x887E_0D7E, // s_or_b32 exec_lo, exec_lo, s13
        0xBF81_0000, // s_endpgm
    ]);
    let native = NativeProgram {
        stage: HwStage::Ms,
        wave64,
        vgprs: 16,
        layout: vec![],
        used: vec![],
        io: StageIo::Ms(MeshIo {
            outputs: vec![],
            clip_cntl: 0,
            threads,
            max_vertices: threads,
            max_primitives: threads,
            topology: 2,
            lds_bytes,
        }),
        code: words
            .iter()
            .flat_map(|word| u32::to_le_bytes(*word))
            .collect(),
    };
    let position = SigElement {
        name: "SV_Position".into(),
        index: 0,
        kind: 3,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 0,
        half: false,
    };
    let osg = super::container::signature(&[position], false);
    let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg)]);
    let container = Container::parse(&bytes)?;
    let program = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
    let structured = structure::structure(&program)?;
    let resources = resource::track(&program, &native.layout, native.stage, Some(&structured))?;
    let raised = io::raise(&program, &native)?;
    emit(
        &container,
        &native,
        &program,
        &structured,
        &raised,
        &resources,
    )
}

#[test]
fn mesh_numeric_wave32_masks_translate_without_a_fixed_size() {
    mesh(64, Ballot::None, false, 0).expect("mesh without a numerical ballot");
    for (threads, native_lds) in [(32, 0), (64, 1024), (128, 0)] {
        let shader =
            mesh(threads, Ballot::Entry, false, native_lds).expect("independent Wave32 ballots");
        let container = Container::parse(&shader.dxil).unwrap();
        let psv = container.part(FourCC::PSV0).unwrap().data;
        let wave_min = crate::dxbc::u32_at(psv, 4 + 16).unwrap();
        let wave_max = crate::dxbc::u32_at(psv, 4 + 20).unwrap();
        assert_eq!((wave_min, wave_max), (0, u32::MAX));
        assert_eq!(
            crate::dxbc::u32_at(psv, 4).unwrap(),
            native_lds + 4 * threads
        );
        #[cfg(windows)]
        if let Ok(mut validator) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            validator
                .validate(&mut shader.dxil.clone())
                .unwrap_or_else(|error| {
                    panic!("{threads} threads, {native_lds} LDS bytes: {error}")
                });
        }
    }
}

#[test]
fn mesh_numeric_wave32_masks_refuse_unproven_barriers_and_excess_scratch() {
    for (ballot, lds, reason) in [
        (
            Ballot::Divergent,
            0,
            "a numerical mesh mask outside the uniform entry block",
        ),
        (
            Ballot::Entry,
            28 * 1024,
            "a numerical mesh mask beyond the groupshared memory limit",
        ),
    ] {
        let actual = mesh(64, ballot, false, lds);
        assert!(
            matches!(&actual, Err(ShaderError::Unsupported { what, .. }) if *what == reason),
            "{actual:?}"
        );
    }
    assert!(mesh(64, Ballot::Entry, true, 0).is_err());
}

#[cfg(windows)]
mod gpu {
    use super::*;
    use crate::emit::{
        bitcode::{Attr, Term},
        host::{Draw, Host, stub_ps},
        shader::Shader,
        validator::Validator,
    };
    use std::ffi::{CStr, CString};
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::core::{Interface, PCSTR};

    fn device() -> (Host, String, ID3D12InfoQueue, bool) {
        let sdk = std::env::var("VICTORY_MESH_MASK_TEST_SDK").expect("Agility 1.620.x path");
        let sdk = CString::new(format!("{}\\", sdk.trim_end_matches(['/', '\\']))).unwrap();
        let adapter = std::env::var("VICTORY_MESH_MASK_TEST_ADAPTER").expect("amd, nvidia or warp");
        // SAFETY: device factory inputs and all D3D12 feature output buffers are live.
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
            let selected: IDXGIAdapter1 = if adapter == "warp" {
                dxgi.EnumWarpAdapter().unwrap()
            } else {
                let vendor = match adapter.as_str() {
                    "amd" => 0x1002,
                    "nvidia" => 0x10de,
                    _ => panic!("unknown adapter {adapter}"),
                };
                (0..)
                    .map_while(|index| dxgi.EnumAdapters1(index).ok())
                    .find(|a| {
                        let desc = a.GetDesc1().unwrap();
                        desc.VendorId == vendor
                            && desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0
                    })
                    .expect("requested hardware adapter")
            };
            let desc = selected.GetDesc1().unwrap();
            assert_eq!(
                desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
                adapter == "warp"
            );
            let name: String = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .into();
            let mut device: Option<ID3D12Device> = None;
            factory
                .CreateDevice(&selected, D3D_FEATURE_LEVEL_12_0, &mut device)
                .unwrap();
            let device = device.unwrap();
            let configured = device
                .cast::<ID3D12DeviceConfiguration>()
                .unwrap()
                .GetDesc();
            assert_ne!(
                configured.Flags.0 & D3D12_DEVICE_FLAG_GPU_BASED_VALIDATION_ENABLED.0,
                0
            );
            assert_eq!(
                configured.GpuBasedValidationFlags
                    & D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING.0 as u32,
                0
            );
            macro_rules! support {
                ($value:ident, $feature:ident) => {
                    device
                        .CheckFeatureSupport(
                            $feature,
                            (&raw mut $value).cast(),
                            size_of_val(&$value) as u32,
                        )
                        .unwrap();
                };
            }
            let mut mesh = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut waves = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            support!(mesh, D3D12_FEATURE_D3D12_OPTIONS7);
            support!(waves, D3D12_FEATURE_D3D12_OPTIONS1);
            support!(model, D3D12_FEATURE_SHADER_MODEL);
            let supported = mesh.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            let info = device.cast().unwrap();
            eprintln!(
                "{name}: mesh tier {:#x}, shader model {:#x}, wave range {}..{}",
                mesh.MeshShaderTier.0,
                model.HighestShaderModel.0,
                waves.WaveLaneCountMin,
                waves.WaveLaneCountMax,
            );
            (Host { device }, name, info, supported)
        }
    }

    fn white_pixel() -> HostShader {
        let mut out = stub_ps();
        let mut pixel = Shader::new(true);
        pixel.outputs.push(SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 0xf,
            half: false,
        });
        for col in 0..4 {
            let args = [
                pixel.m.i32(0),
                pixel.m.i32(0),
                pixel.m.i8(col),
                pixel.m.f32(1.0_f32.to_bits()),
            ];
            pixel.op(
                pixel.entry,
                "storeOutput.f32",
                Attr::NoUnwind,
                pixel.t.void,
                5,
                &args,
            );
        }
        pixel.m.terminate(pixel.entry, Term::Ret);
        out.dxil = pixel.finish();
        out
    }

    fn check_debug(info: &ID3D12InfoQueue) {
        let mut errors = Vec::new();
        // SAFETY: SDK message records are eight-byte aligned, and sizes
        // come from the queue before we allocate each temporary buffer.
        unsafe {
            for index in 0..info.GetNumStoredMessages() {
                let mut size = 0;
                info.GetMessage(index, None, &mut size).unwrap();
                let mut bytes = vec![0u64; size.div_ceil(8)];
                let message = bytes.as_mut_ptr().cast::<D3D12_MESSAGE>();
                info.GetMessage(index, Some(message), &mut size).unwrap();
                if (*message).Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
                    errors.push(
                        CStr::from_ptr((*message).pDescription.cast())
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    #[ignore = "requires Agility 1.620.x, dxil.dll, a named adapter and Graphics Tools"]
    fn mesh_numeric_wave32_masks_control_two_triangles() {
        let (host, name, info, supported) = device();
        if !supported {
            eprintln!("{name}: mesh rasterization unavailable");
            return;
        }
        let pixel = white_pixel();
        let mut validator = Validator::load(Validator::default_path().as_deref()).unwrap();
        validator.validate(&mut pixel.dxil.clone()).unwrap();
        for ballot in [Ballot::None, Ballot::Entry] {
            let mesh = mesh(64, ballot, false, 0).unwrap();
            validator.validate(&mut mesh.dxil.clone()).unwrap();
            let (root, pso) = host.mesh_pipeline(&mesh, &pixel).unwrap();
            let pixels = host
                .draw(
                    &root,
                    &pso,
                    &Draw {
                        roots: vec![],
                        textures: &[],
                        samplers: &[],
                        vertices: &[],
                        stride: 0,
                        size: (16, 16),
                        mesh_groups: 1,
                    },
                )
                .unwrap();
            let probes = [pixels[13 * 16 + 2], pixels[13 * 16 + 10]];
            assert_eq!(probes, [[1.; 4]; 2], "{name}, ballot={ballot:?}");
            eprintln!("{name}: ballot={ballot:?}, independent triangle probes {probes:?}");
        }
        check_debug(&info);
    }
}
