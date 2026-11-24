use super::*;
use std::ffi::c_void;
use victory_shader::native::{ComputeIo, StageIo};
use victory_shader::{
    Container, HwStage, NativeProgram, RootParam, emit, io, ir, resource, structure,
};
use windows::core::PCSTR;

const WORDS: usize = 128;

const FIRST: usize = 16;
const COUNT: usize = 4;

struct TitleWords(Box<[u32; WORDS]>, bool);

impl TitleWords {
    fn new(words: [u32; WORDS], nested: bool) -> Self {
        let title = Self(Box::new(words), nested);
        let base = title.base();
        crate::shadow::register(base as usize, base, (WORDS * 4) as u64);
        if nested {
            let start = base + (FIRST * 4) as u64;
            crate::shadow::register(start as usize, start, (COUNT * 4) as u64);
        }
        title
    }

    fn base(&self) -> u64 {
        self.0.as_ptr() as u64
    }

    fn view(&self, slot: u64, uav: bool) {
        let mut buffer = [0; 14];
        buffer[0] = D3D12_RESOURCE_DIMENSION_BUFFER.0 as u32;
        buffer[4] = (WORDS * 4) as u32;
        let mut desc = [0; 16];
        desc[0] = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS.0 as u32;
        desc[1] = 1; // BUFFER for both SRV and UAV
        if uav {
            desc[2] = FIRST as u32;
            desc[4] = COUNT as u32;
            desc[8] = D3D12_BUFFER_UAV_FLAG_RAW.0 as u32;
        } else {
            desc[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
            desc[4] = FIRST as u32;
            desc[6] = COUNT as u32;
            desc[8] = D3D12_BUFFER_SRV_FLAG_RAW.0 as u32;
        }
        desc[10] = self.base() as u32;
        desc[11] = (self.base() >> 32) as u32;
        if uav {
            crate::descriptor::create_placed_uav(std::ptr::null(), &buffer, &desc, slot);
        } else {
            crate::descriptor::create_placed_srv(std::ptr::null(), &buffer, &desc, slot);
        }
    }
}

impl Drop for TitleWords {
    fn drop(&mut self) {
        if self.1 {
            let start = self.base() + (FIRST * 4) as u64;
            crate::shadow::unregister(start as usize, start);
        }
        crate::shadow::unregister(self.base() as usize, self.base());
    }
}

struct TestHeap(This, u64);

impl TestHeap {
    fn new(sampler: bool) -> Self {
        let (heap, base) = crate::descriptor::test_heap(sampler);
        Self(heap, base)
    }
}

impl Drop for TestHeap {
    fn drop(&mut self) {
        crate::object::slot::<extern "system" fn(This) -> u32>(self.0, 16)(self.0);
    }
}

fn pipeline(fresh: bool) -> HostPipeline {
    let words = [
        0xBE85_0380u32, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // load descriptor at table + 0
        0xF408_0302,
        0xFA00_0020, // load descriptor at table + 32
        0xE030_0000,
        0x8002_0200, // buffer_load_dword v2 from s[8:11], offset 0
        0xE070_0000,
        0x8003_0200, // buffer_store_dword v2 to s[12:15], offset 0
        0xBF81_0000, // s_endpgm
    ];
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 6,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 4,
            slot: 0,
        }],
        used: [(ResourceType::Srv, 0), (ResourceType::Uav, 8)]
            .into_iter()
            .chain(fresh.then_some((ResourceType::Cbv, 16)))
            .map(|(kind, offset)| UsedResource {
                kind,
                root: RootType::Table,
                table_offset_dwords: offset,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 4,
                slot: 0,
            })
            .collect(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
    };
    let program = ir::build(&native.code, false, native.vgprs).unwrap();
    let structured = structure::structure(&program).unwrap();
    let resources =
        resource::track(&program, &native.layout, native.stage, Some(&structured)).unwrap();
    let stage = io::raise(&program, &native).unwrap();
    let container = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let mut host = emit::emit(
        &container,
        &native,
        &program,
        &structured,
        &stage,
        &resources,
    )
    .unwrap();
    emit::validator::Validator::load(emit::validator::Validator::default_path().as_deref())
        .expect("DXIL validator")
        .validate(&mut host.dxil)
        .expect("translated DXIL validates");
    HostPipeline::new(
        &[crate::pipeline::test_compute_shader(host, native.used)],
        None,
    )
    .expect("compute pipeline")
}

unsafe extern "system" fn note_error(
    _: D3D12_MESSAGE_CATEGORY,
    severity: D3D12_MESSAGE_SEVERITY,
    _: D3D12_MESSAGE_ID,
    text: PCSTR,
    context: *mut c_void,
) {
    if severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
        // SAFETY: registered with a live counter and a D3D12 message.
        let (errors, text) = unsafe {
            (
                &*context.cast::<AtomicU64>(),
                text.to_string().unwrap_or_default(),
            )
        };
        errors.fetch_add(1, Ordering::Relaxed);
        eprintln!("{text}");
    }
}

#[test]
#[ignore = "requires Agility 1.620.2 and VICTORY_TEST_BUFFER_VIEW_ADAPTER"]
fn table_buffer_views_refresh_the_shadow_the_descriptor_reads() {
    assert!(std::env::var_os("VICTORY_TEST_BUFFER_VIEW_ADAPTER").is_some());
    let host = crate::host::host().expect("configured D3D12 test device");
    static ERRORS: AtomicU64 = AtomicU64::new(0);
    let validation: ID3D12InfoQueue1 = host.device.cast().unwrap();
    let mut cookie = 0;
    // SAFETY: the callback and its counter live for the process.
    unsafe {
        validation
            .RegisterMessageCallback(
                Some(note_error),
                D3D12_MESSAGE_CALLBACK_FLAG_NONE,
                std::ptr::from_ref(&ERRORS).cast_mut().cast(),
                &mut cookie,
            )
            .unwrap();
    }
    let views = TestHeap::new(false);
    let samplers = TestHeap::new(true);
    let mut failures = Vec::new();
    for fresh in [false, true] {
        let pipeline = pipeline(fresh);
        for (nested_input, nested_output) in [(false, false), (true, false), (false, true)] {
            let input = TitleWords::new(std::array::from_fn(|i| 1000 + i as u32), nested_input);
            let output = TitleWords::new([17; WORDS], nested_output);
            input.view(views.1, false);
            output.view(views.1 + 32, true);
            let mut state = State::default();
            state.set_heaps(&[views.0, samplers.0]);
            state.set_table(true, 0, views.1);
            let list = host.new_list().unwrap();
            assert!(state.bind(&list, &pipeline, true));
            // SAFETY: a graphics-capable command list with a compute pipeline and its roots.
            unsafe { list.list.Dispatch(1, 1, 1) };
            list.close();
            host.execute(&[&list]);
            let mut expected = [17; WORDS];
            expected[FIRST] = input.0[FIRST];
            if *output.0 != expected {
                failures.push((fresh, nested_input, nested_output, output.0[FIRST]));
            }
        }
    }
    // SAFETY: this cookie identifies the registered callback above.
    unsafe { validation.UnregisterMessageCallback(cookie).unwrap() };
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
    assert!(
        failures.is_empty(),
        "(fresh, nested SRV, nested UAV, readback): {failures:?}"
    );
}
