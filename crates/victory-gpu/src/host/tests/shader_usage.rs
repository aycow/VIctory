use super::*;
use crate::object::{S_OK, This, guid, slot};
use crate::pipeline::host::{GraphicsState, HostPipeline};
use victory_shader::{HwStage, emit::HostShader};
use windows::Win32::Graphics::Direct3D::{Fxc::D3DCompile, ID3DBlob};
use windows::core::{PCSTR, s};

const SHADERS: &str = r#"
float4 vertex() : SV_Position { return float4(0, 0, 0, 1); }
float4 pixel() : SV_Target { return float4(0, 0, 0, 1); }
"#;

struct TitleHeap(This);

impl TitleHeap {
    fn new(kind: u32) -> Self {
        let iid = guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        // Console heap description followed by zeroed placement words.
        let desc = [kind, 32, 1, 0, 0, 0, 0, 0];
        let mut handle = std::ptr::null();
        assert_eq!(
            crate::descriptor::create_heap(
                std::ptr::null(),
                desc.as_ptr().cast(),
                &iid,
                &mut handle,
            ),
            S_OK,
        );
        Self(handle)
    }
}

impl Drop for TitleHeap {
    fn drop(&mut self) {
        slot::<extern "system" fn(This) -> u32>(self.0, 0x10)(self.0);
    }
}

fn shader(
    stage: HwStage,
    entry: PCSTR,
    target: PCSTR,
    heap_buffer_srvs: HeapBufferSrvUsage,
) -> HostShader {
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: the original test source, names, and output pointer live
    // through compilation. We copy the resulting blob before releasing it.
    let dxil = unsafe {
        D3DCompile(
            SHADERS.as_ptr().cast(),
            SHADERS.len(),
            s!("heap usage test"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            None,
        )
        .unwrap();
        let code = code.unwrap();
        std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize()).to_vec()
    };
    HostShader {
        stage,
        dxil,
        user_data: Vec::new(),
        samplers: Vec::new(),
        static_samplers: Vec::new(),
        root_cbvs: Vec::new(),
        root_srvs: Vec::new(),
        heap_buffer_srvs,
    }
}

fn pipeline(
    vertex: HeapBufferSrvUsage,
    pixel: HeapBufferSrvUsage,
    topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE,
) -> HostPipeline {
    // Real VS and PS bytecode, with explicit metadata fixtures so this checks
    // runtime selection independently of the shader-analysis unit tests.
    let vs = shader(HwStage::Vs, s!("vertex"), s!("vs_5_1"), vertex);
    let ps = shader(HwStage::Ps, s!("pixel"), s!("ps_5_1"), pixel);
    let mut state = GraphicsState::default();
    state.topology = topology;
    state.targets.NumRenderTargets = 1;
    state.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
    let depth_format = [16, DXGI_FORMAT_D32_FLOAT.0 as u32];
    // SAFETY: the two dwords hold a complete depth-format subobject.
    unsafe { state.read(16, depth_format.as_ptr().cast()) };
    HostPipeline::new(
        &[
            crate::pipeline::test_translated_shader("vertex", vs),
            crate::pipeline::test_translated_shader("pixel", ps),
        ],
        Some(&state),
    )
    .expect("synthetic graphics pipeline")
}

fn fixture_host() -> Option<&'static Host> {
    let strict = std::env::var_os("VICTORY_TEST_SRV_GBV").is_some();
    let Some(gpu) = host() else {
        assert!(!strict, "a D3D12 device is required for GPU validation");
        return None;
    };
    let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
        HighestShaderModel: D3D_SHADER_MODEL_6_6,
    };
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    // SAFETY: these feature queries write the two correctly sized structures.
    let supported = unsafe {
        gpu.device
            .CheckFeatureSupport(
                D3D12_FEATURE_SHADER_MODEL,
                (&raw mut model).cast(),
                size_of_val(&model) as u32,
            )
            .is_ok()
            && gpu
                .device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS,
                    (&raw mut options).cast(),
                    size_of_val(&options) as u32,
                )
                .is_ok()
    } && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0
        && options.ResourceBindingTier.0 >= D3D12_RESOURCE_BINDING_TIER_3.0;
    if !supported {
        assert!(
            !strict,
            "expected SM 6.6 and binding tier 3 with GPU validation requested"
        );
        eprintln!("skipped direct SRV binding fixture: SM 6.6 and binding tier 3 required");
        return None;
    }
    Some(gpu)
}

#[cfg(victory_test_agility)]
#[test]
fn agility_runtime_meets_the_requested_version() {
    use windows::core::Interface;

    let gpu = host().expect("a D3D12 device with the staged runtime");
    let config = gpu
        .device
        .cast::<ID3D12DeviceConfiguration>()
        .expect("device SDK configuration");
    // SAFETY: read-only query of a live device. The loader may choose a newer
    // inbox SDK over the staged runtime when both are installed.
    let active = unsafe { config.GetDesc().SDKVersion };
    let requested = crate::agility::D3D12SDKVersion;
    assert!(
        active >= requested,
        "active D3D12 SDK version {active} is older than requested version {requested}"
    );
    eprintln!("GPU test process active D3D12 SDK version {active} (requested {requested})");
}

#[test]
fn bind_keeps_shader_srv_usage_for_stable_view_heap() {
    let Some(gpu) = fixture_host() else {
        return;
    };
    let title_heap = TitleHeap::new(0);
    let sampler_heap = TitleHeap::new(1);
    let native_heap = crate::descriptor::heap(title_heap.0).host().unwrap();
    use HeapBufferSrvUsage::{All, Exact, None};
    let none = pipeline(None, None, D3D12_PRIMITIVE_TOPOLOGY_TYPE_POINT);
    let exact = pipeline(
        Exact(vec![5, 7]),
        Exact(vec![2, 5]),
        D3D12_PRIMITIVE_TOPOLOGY_TYPE_LINE,
    );
    let all = pipeline(Exact(vec![12]), All, D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE);
    assert_ne!(none.pso.as_raw(), exact.pso.as_raw());
    assert_ne!(exact.pso.as_raw(), all.pso.as_raw());

    let list = gpu.new_list().unwrap();
    let mut state = crate::draw::State::default();
    state.set_heaps(&[sampler_heap.0, title_heap.0]);
    let check = |usage| {
        let deferred = list.deferred();
        let [heap] = deferred.view_heaps.as_slice() else {
            panic!("expected one retained view heap");
        };
        assert!(std::sync::Arc::ptr_eq(&heap.heap, native_heap));
        assert_eq!(heap.usage, usage);
    };
    state.bind(&list, &none, false);
    check(None);
    state.bind(&list, &exact, false);
    check(Exact(vec![2, 5, 7]));
    state.bind(&list, &none, false);
    state.bind(&list, &exact, false);
    check(Exact(vec![2, 5, 7]));
    state.bind(&list, &all, false);
    state.bind(&list, &exact, false);
    check(All);
    list.close();

    // SAFETY: the factory and adapter queries refer to the live device.
    let adapter_name = unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByLuid(gpu.device.GetAdapterLuid())
            .unwrap();
        String::from_utf16_lossy(&adapter.GetDesc1().unwrap().Description)
    };
    eprintln!(
        "SRV bind selection recorded on {}",
        adapter_name.trim_end_matches('\0')
    );
    let errors = gpu.log_messages();
    assert!(
        errors.is_none(),
        "D3D12 pipeline or binding errors: {errors:?}"
    );
}

#[test]
fn no_buffer_srv_use_keeps_native_descriptor_owners_alive() {
    let Some(gpu) = fixture_host() else { return };
    let heap = TitleHeap::new(0);
    let sampler = TitleHeap::new(1);
    let weak = std::sync::Arc::downgrade(crate::descriptor::heap(heap.0).host().unwrap());
    let pipeline = pipeline(
        HeapBufferSrvUsage::None,
        HeapBufferSrvUsage::None,
        D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
    );
    let list = gpu.new_list().unwrap();
    let mut state = crate::draw::State::default();
    state.set_heaps(&[heap.0, sampler.0]);
    state.bind(&list, &pipeline, false);
    list.close();
    drop(heap);
    assert!(
        weak.upgrade().is_some(),
        "a list still owns the native resources referenced by its view heap"
    );
    drop(list);
    assert!(
        weak.upgrade().is_none(),
        "closing title heap and list frees their native owner"
    );
}
