//! Bounded source-built mesh projection control for the pipeline counters.

use super::HostShader;
use super::bitcode::{Attr, Term, pred};
use super::container::SigElement;
use super::host::{Draw, Host, stub_ps};
use super::shader::{Mesh, Shader};
use super::validator::Validator;
use crate::native::HwStage;
use std::ffi::{CStr, CString};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCSTR};

fn device() -> (Host, String) {
    let sdk = std::env::var("VICTORY_MESH_MASK_TEST_SDK").expect("Agility 1.620.x path");
    let sdk = CString::new(format!("{}\\", sdk.trim_end_matches(['/', '\\']))).unwrap();
    let adapter = std::env::var("VICTORY_MESH_MASK_TEST_ADAPTER").expect("amd, nvidia or warp");
    // SAFETY: SDK path, feature descriptions and output slots are live.
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
                .find(|a| a.GetDesc1().unwrap().VendorId == vendor)
                .expect("requested hardware adapter")
        };
        let desc = selected.GetDesc1().unwrap();
        assert_eq!(
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
            adapter == "warp"
        );
        let name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_owned();
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
        let mut mesh = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS7,
                (&raw mut mesh).cast(),
                size_of_val(&mesh) as u32,
            )
            .unwrap();
        assert_eq!(mesh.MeshShaderTier, D3D12_MESH_SHADER_TIER_1);
        (Host { device }, name)
    }
}

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
        usage: 0xf,
        half: false,
    }
}

fn pixel() -> HostShader {
    let mut out = stub_ps();
    let mut shader = Shader::new(true);
    shader.outputs.push(signature("SV_Target", 16));
    for col in 0..4 {
        let args = [
            shader.m.i32(0),
            shader.m.i32(0),
            shader.m.i8(col),
            shader.m.f32(1.0_f32.to_bits()),
        ];
        shader.op(
            shader.entry,
            "storeOutput.f32",
            Attr::NoUnwind,
            shader.t.void,
            5,
            &args,
        );
    }
    shader.m.terminate(shader.entry, Term::Ret);
    out.dxil = shader.finish();
    out
}

fn mesh(w: f32) -> HostShader {
    let mut out = stub_ps();
    out.stage = HwStage::Ms;
    let mut shader = Shader::new(false);
    shader.threads = Some([3, 1, 1]);
    shader.mesh = Some(Mesh {
        max_vertices: 3,
        max_primitives: 1,
        topology: 2,
    });
    shader.outputs.push(signature("SV_Position", 3));
    let at = shader.entry;
    let [zero, one, two, three] = [0, 1, 2, 3].map(|value| shader.m.i32(value));
    shader.op(
        at,
        "setMeshOutputCounts",
        Attr::NoUnwind,
        shader.t.void,
        168,
        &[three, one],
    );
    let lane = shader.op(
        at,
        "flattenedThreadIdInGroup.i32",
        Attr::ReadNone,
        shader.t.i32,
        96,
        &[],
    );
    let [low, high, depth, w] = [-0.5, 1.0, 0.5 * w, w].map(|value| shader.m.f32(value.to_bits()));
    let right = shader.m.cmp(at, pred::EQ, lane, one);
    let top = shader.m.cmp(at, pred::EQ, lane, two);
    let x = shader.m.select(at, right, high, low);
    let y = shader.m.select(at, top, high, low);
    for (col, value) in [x, y, depth, w].into_iter().enumerate() {
        let col = shader.m.i8(col as u8);
        shader.op(
            at,
            "storeVertexOutput.f32",
            Attr::NoUnwind,
            shader.t.void,
            171,
            &[zero, zero, col, value, lane],
        );
    }
    let leader = shader.m.cmp(at, pred::EQ, lane, zero);
    let indices = shader.m.new_block();
    let end = shader.m.new_block();
    shader.m.terminate(at, Term::CondBr(leader, indices, end));
    shader.op(
        indices,
        "emitIndices",
        Attr::NoUnwind,
        shader.t.void,
        169,
        &[zero, zero, one, two],
    );
    shader.m.terminate(indices, Term::Br(end));
    shader.m.terminate(end, Term::Ret);
    out.dxil = shader.finish();
    out
}

#[test]
#[ignore = "requires named Agility mesh-tier adapter with Graphics Tools"]
fn clip_space_w_changes_pixel_work_at_fixed_mesh_output_counts() {
    let (host, name) = device();
    let mut validator = Validator::load(Validator::default_path().as_deref()).unwrap();
    let pixel = pixel();
    validator.validate(&mut pixel.dxil.clone()).unwrap();
    let draw = Draw {
        roots: vec![],
        textures: &[],
        samplers: &[],
        vertices: &[],
        stride: 0,
        size: (32, 32),
        mesh_groups: 64,
    };
    let results = [1.0, 0.5, 0.125, -1.0].map(|w| {
        let mesh = mesh(w);
        validator.validate(&mut mesh.dxil.clone()).unwrap();
        let (root, pso) = host.mesh_pipeline(&mesh, &pixel).unwrap();
        let (pixels, stats) = host.draw_with_stats(&root, &pso, &draw).unwrap();
        let coverage = pixels.iter().filter(|pixel| **pixel == [1.; 4]).count();
        eprintln!(
            "{name}: groups={} authored_triangles={} W={w}, CInvocations={} CPrimitives={} PSInvocations={} coverage={coverage}/{}",
            draw.mesh_groups, draw.mesh_groups, stats.CInvocations, stats.CPrimitives, stats.PSInvocations, pixels.len()
        );
        (coverage, stats.PSInvocations)
    });
    assert!(results[0].0 > 0 && results[0].1 > 0);
    assert!(results[0].0 < results[1].0 && results[0].1 < results[1].1);
    assert!(results[1].0 < results[2].0 && results[1].1 < results[2].1);
    assert_eq!(results[2].0, 32 * 32);
    assert_eq!(results[3], (0, 0));
    let info = host.device.cast::<ID3D12InfoQueue>().unwrap();
    // SAFETY: message storage is aligned and sized by the debug queue.
    unsafe {
        for index in 0..info.GetNumStoredMessages() {
            let mut size = 0;
            info.GetMessage(index, None, &mut size).unwrap();
            let mut storage = vec![0u64; size.div_ceil(8)];
            let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
            info.GetMessage(index, Some(message), &mut size).unwrap();
            assert!(
                (*message).Severity.0 > D3D12_MESSAGE_SEVERITY_ERROR.0,
                "{:?}",
                CStr::from_ptr((*message).pDescription.cast())
            );
        }
    }
}
