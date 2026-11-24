use super::bitcode::{Attr, Term};
use super::container::SigElement;
use super::host::{Draw, Host, Root};
use super::shader::Shader;
use super::validator::Validator;
use super::{HostShader, emit};
use crate::native::StageIo;
use crate::{
    Container, FourCC, HwStage, NativeProgram, RootParam, RootType, io, ir, resource, structure,
};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

fn signature(name: &str, kind: u8) -> SigElement {
    SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    }
}

fn vertex() -> HostShader {
    let mut shader = Shader::new(false);
    shader.inputs.push(signature("POSITION", 0));
    shader.outputs.push(signature("SV_Position", 3));
    let b = shader.entry;
    for component in 0..4 {
        let row = shader.m.i32(0);
        let col = shader.m.i8(component);
        let vertex = shader.m.undef(shader.t.i32);
        let value = shader.op(
            b,
            "loadInput.f32",
            Attr::ReadNone,
            shader.t.f32,
            4,
            &[row, row, col, vertex],
        );
        shader.op(
            b,
            "storeOutput.f32",
            Attr::NoUnwind,
            shader.t.void,
            5,
            &[row, row, col, value],
        );
    }
    shader.m.terminate(b, Term::Ret);
    HostShader {
        stage: HwStage::Vs,
        dxil: shader.finish(),
        user_data: vec![],
        samplers: vec![],
        static_samplers: vec![],
        root_cbvs: vec![],
        root_srvs: vec![],
    }
}

fn translated_pixel() -> HostShader {
    let words = [
        0xbe81_0380u32, // s_mov_b32 s1, 0
        0xbe83_0380,    // s_mov_b32 s3, 0
        0xf40c_0200,
        0xfa00_0000, // s_load_dwordx8 s[8:15], s[0:1], 0
        0xf40c_0400,
        0xfa00_0020, // s_load_dwordx8 s[16:23], s[0:1], 32
        0xf408_0601,
        0xfa00_0000, // s_load_dwordx4 s[24:27], s[2:3], 0
        0x7e04_02f0, // v_mov_b32 v2, 0.5
        0x7e06_02f0, // v_mov_b32 v3, 0.5
        0xf09c_0f08,
        0x00c2_0402, // sample colour with v[2:3], at v[4:7]
        0xf09c_0f08,
        0x00c4_0802, // sample mask with v[2:3], at v[8:11]
        0x0800_10f2, // v_sub_f32 v0, 1.0, v8
        0x0e18_0900, // v_mul_legacy_f32 v12, v0, v4
        0x0e1a_0b00, // v_mul_legacy_f32 v13, v0, v5
        0x0e1c_0d00, // v_mul_legacy_f32 v14, v0, v6
        0x0e1e_0f00, // v_mul_legacy_f32 v15, v0, v7
        0xf800_180f,
        0x0f0e_0d0c, // exp mrt0 v12..v15 done vm
        0xbf81_0000, // s_endpgm
    ];
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 16,
        layout: [0, 2]
            .into_iter()
            .map(|user_data| RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data,
                slot: user_data / 2,
            })
            .collect(),
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        },
        code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let osg1 = super::container::signature(&[signature("SV_Target", 16)], false);
    let container = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
    let c = Container::parse(&container).unwrap();
    emit(&c, &native, &p, &st, &io, &res).unwrap()
}

#[test]
#[ignore = "requires Agility SDK 1.620.2 and an explicit adapter"]
fn two_pixel_samples_feed_legacy_material_colour() {
    let which = std::env::var("VICTORY_TEST_MATERIAL_ADAPTER").expect("adapter");
    let host = material_test_device(&which);
    let queue: ID3D12InfoQueue = host.device.cast().expect("debug layer");
    let (mut vs, mut ps) = (vertex(), translated_pixel());
    let mut validator =
        Validator::load(Validator::default_path().as_deref()).expect("DXIL validator");
    validator
        .validate(&mut vs.dxil)
        .expect("synthetic vertex DXIL validates");
    validator
        .validate(&mut ps.dxil)
        .expect("translated pixel DXIL validates");
    let (root, pso) = host.pipeline(&vs, &ps).unwrap();
    let sampler = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        MaxLOD: f32::MAX,
        ..Default::default()
    };
    let vertices = [
        -1.0, -1.0, 0.5, 1.0, -1.0, 3.0, 0.5, 1.0, 3.0, -1.0, 0.5, 1.0,
    ];
    for (color, x) in [
        ([0.5f32, 0.25, 1.0, 0.75], 0.0f32),
        ([0.5, 0.25, 1.0, 0.75], 0.25),
        ([0.5, 0.25, 1.0, 0.75], 1.0),
        ([f32::INFINITY, -0.25, 0.75, f32::NEG_INFINITY], 1.0),
        ([0.0, 0.0, 0.0, 0.0], 0.375),
    ] {
        let colour = [color];
        let mask = [[x, 17.0, 23.0, 31.0]];
        let out = host
            .draw(
                &root,
                &pso,
                &Draw {
                    roots: vec![
                        (0, Root::Constants(vec![0])),
                        (1, Root::Constants(vec![0; 3])),
                    ],
                    textures: &[(1, 1, &colour), (1, 1, &mask)],
                    samplers: &[sampler],
                    vertices: &vertices,
                    stride: 16,
                    size: (2, 2),
                    mesh_groups: 0,
                },
            )
            .unwrap();
        let expected = color.map(|value| {
            if x == 1.0 || value == 0.0 {
                0.0
            } else {
                (1.0 - x) * value
            }
        });
        assert_eq!(out.len(), 4);
        for texel in out {
            assert_eq!(
                texel.map(f32::to_bits),
                expected.map(f32::to_bits),
                "{which} x={x} color={color:?}"
            );
        }
    }
    // SAFETY: the owned queue and each aligned output buffer outlive the query.
    unsafe {
        for i in 0..queue.GetNumStoredMessages() {
            let mut len = 0;
            queue.GetMessage(i, None, &mut len).unwrap();
            let mut bytes = vec![0u64; len.div_ceil(8)];
            let msg = bytes.as_mut_ptr().cast::<D3D12_MESSAGE>();
            queue.GetMessage(i, Some(msg), &mut len).unwrap();
            let msg = &*msg;
            assert!(
                msg.Severity.0 > D3D12_MESSAGE_SEVERITY_ERROR.0,
                "{which}: {}",
                std::ffi::CStr::from_ptr(msg.pDescription.cast()).to_string_lossy()
            );
        }
    }
    eprintln!("{which}: material colour and zero controls match every pixel");
}

fn material_test_device(which: &str) -> Host {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::core::Interface;

    let vendor = match which {
        "amd" => Some(0x1002),
        "nvidia" => Some(0x10de),
        "warp" => None,
        _ => panic!("unexpected adapter {which}"),
    };
    let host = material_test_host(&material_test_factory(true), vendor);
    let configuration: ID3D12DeviceConfiguration = host.device.cast().unwrap();
    // SAFETY: a read-only configuration of the live device.
    let configuration = unsafe { configuration.GetDesc() };
    assert_eq!(configuration.SDKVersion, 620);
    assert_ne!(
        configuration.Flags.0 & D3D12_DEVICE_FLAG_GPU_BASED_VALIDATION_ENABLED.0,
        0
    );
    assert_eq!(configuration.GpuBasedValidationFlags, 0);
    host
}

fn material_test_factory(
    validate: bool,
) -> windows::Win32::Graphics::Direct3D12::ID3D12DeviceFactory {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::core::s;

    // SAFETY: both configuration interfaces receive pointers to their SDK's live GUIDs.
    unsafe {
        let mut configuration: Option<ID3D12SDKConfiguration1> = None;
        D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut configuration).unwrap();
        let factory: ID3D12DeviceFactory = configuration
            .unwrap()
            .CreateDeviceFactory(620, s!(".\\D3D12\\"))
            .unwrap();
        if validate {
            let debug: ID3D12Debug1 = factory
                .GetConfigurationInterface(&CLSID_D3D12Debug)
                .unwrap();
            debug.EnableDebugLayer();
            debug.SetEnableGPUBasedValidation(true);
        }
        factory
    }
}

fn material_test_host(
    device_factory: &windows::Win32::Graphics::Direct3D12::ID3D12DeviceFactory,
    vendor: Option<u32>,
) -> Host {
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::*;

    // SAFETY: the test selects an adapter with DXGI and all output pointers stay live.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = match vendor {
            None => factory.EnumWarpAdapter().unwrap(),
            Some(vendor) => (0..)
                .map_while(|i| factory.EnumAdapters1(i).ok())
                .find(|a| a.GetDesc1().unwrap().VendorId == vendor)
                .expect("requested adapter"),
        };
        let desc = adapter.GetDesc1().unwrap();
        assert_eq!(
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
            vendor.is_none()
        );
        let name = String::from_utf16_lossy(&desc.Description);
        eprintln!("adapter: {}", name.trim_end_matches('\0'));
        let mut device: Option<ID3D12Device> = None;
        device_factory
            .CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)
            .unwrap();
        Host {
            device: device.unwrap(),
        }
    }
}
