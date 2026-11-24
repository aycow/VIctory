use super::*;
use std::ffi::c_void;
use victory_shader::native::{ComputeIo, StageIo};
use victory_shader::{
    Container, HwStage, NativeProgram, RootParam, emit, io, ir, resource, structure,
};
use windows::core::PCSTR;

const WORDS: usize = 128;

struct TitleWords(Box<[u32; WORDS]>);

impl TitleWords {
    fn new(words: [u32; WORDS]) -> Self {
        let title = Self(Box::new(words));
        let base = title.base();
        crate::shadow::register(base as usize, base, (WORDS * 4) as u64);
        title
    }

    fn base(&self) -> u64 {
        self.0.as_ptr() as u64
    }

    fn view(&self, slot: u64, stride: u32, uav: bool, typed: bool) {
        let mut buffer = [0; 14];
        buffer[0] = D3D12_RESOURCE_DIMENSION_BUFFER.0 as u32;
        buffer[4] = (WORDS * 4) as u32;
        let mut desc = [0; 16];
        desc[0] = if typed { 2 } else { 0 }; // RGBA32_FLOAT or UNKNOWN
        desc[1] = 1; // BUFFER for both SRV and UAV
        if uav {
            desc[2] = 4; // first record
            desc[4] = 4; // record count
            desc[5] = stride;
        } else {
            desc[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
            desc[4] = 4; // first record; 16-byte-aligned for all tested strides
            desc[6] = 4; // record count
            desc[7] = stride;
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

fn pipeline(
    [read_offset, store_offset, atomic_offset]: [u32; 3],
    width: usize,
    target: emit::Target,
) -> Option<HostPipeline> {
    let load = [0x0c, 0x0d, 0x0f, 0x0e][width - 1];
    let store = [0x1c, 0x1d, 0x1f, 0x1e][width - 1];
    let words = [
        0xBE85_0380u32, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // load descriptor at table + 0
        0xF408_0302,
        0xFA00_0020, // load descriptor at table + 32
        0x7E00_0200, // v_mov_b32 v0, s0; read index
        0xE000_2000 | (load << 18) | read_offset,
        0x8002_0200, // indexed dword load
        0x7E00_0201, // v_mov_b32 v0, s1; store index
        0xE000_2000 | (store << 18) | store_offset,
        0x8003_0200, // indexed dword store
        0x7E00_0202, // v_mov_b32 v0, s2; atomic index
        0x7E06_028B, // v_mov_b32 v3, 11
        0xE0C8_2000 | atomic_offset,
        0x8003_0300, // indexed atomic add
        0xBF81_0000, // s_endpgm
    ];
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 6,
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
                stride: 0,
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
    let program = ir::build(&native.code, false, native.vgprs).unwrap();
    let resources = resource::track(&program, &native.layout, native.stage).unwrap();
    let stage = io::raise(&program, &native).unwrap();
    let structured = structure::structure(&program).unwrap();
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
        target,
    )
    .unwrap();
    emit::validator::Validator::load(emit::validator::Validator::default_path().as_deref())
        .expect("DXIL validator")
        .validate(&mut host.dxil)
        .expect("translated DXIL validates");
    assert_eq!(host.stride_views.len(), 2);
    HostPipeline::new(
        &[crate::pipeline::test_compute_shader(host, native.used)],
        None,
    )
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
#[ignore = "requires Agility 1.620.2 and VICTORY_TEST_STRIDE_ADAPTER"]
fn runtime_stride_aliases_production_bind() {
    assert!(std::env::var_os("VICTORY_TEST_STRIDE_ADAPTER").is_some());
    if !crate::pipeline::host::runtime_stride_aliases_enabled() {
        assert!(
            pipeline([4, 8, 0], 1, emit::Target::ANY).is_none(),
            "default path must refuse runtime stride"
        );
        eprintln!("runtime stride pipeline refused with opt-in disabled");
        return;
    }
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
    let workloads = [
        ([4, 8, 0], 1, [2, 1, 2], [12, 20]),
        ([12, 8, 12], 1, [2, 1, 2], [12, 12]),
        ([4, 8, 0], 1, [u32::MAX, 4, 4], [12, 12]),
        ([0, 0, 0], 2, [2, 1, 2], [8, 20]),
        ([4, 0, 0], 3, [2, 1, 2], [12, 20]),
        ([0, 0, 0], 4, [2, 1, 2], [16, 32]),
    ];
    for (case, (offsets, width, indices, strides)) in workloads.into_iter().enumerate() {
        let p = pipeline(offsets, width, host.target()).expect("opt-in pipeline");
        let title = TestHeap::new(false);
        let sampler = TestHeap::new(true);
        let inputs = [
            TitleWords::new(std::array::from_fn(|i| 100 + i as u32)),
            TitleWords::new(std::array::from_fn(|i| 200 + i as u32)),
        ];
        let outputs = [TitleWords::new([17; WORDS]), TitleWords::new([23; WORDS])];
        let mut state = State::default();
        state.set_heaps(&[title.0, sampler.0]);
        state.set_table(true, 1, title.1);
        state.set_constants(true, 0, &indices, 0);
        let lists = [host.new_list().unwrap(), host.new_list().unwrap()];
        for (n, &stride) in strides.iter().enumerate() {
            inputs[n].view(title.1, stride, false, false);
            outputs[n].view(title.1 + 32, stride, true, false);
            assert!(state.bind(&lists[n], &p, true));
            // SAFETY: the translated pipeline and both aliases are bound and retained.
            unsafe { lists[n].list.Dispatch(1, 1, 1) };
            lists[n].close();
            state = State::default();
            state.set_heaps(&[title.0, sampler.0]);
            state.set_table(true, 1, title.1);
            state.set_constants(true, 0, &indices, 0);
        }
        inputs[0].view(title.1, 0, false, true);
        let refused = host.new_list().unwrap();
        assert!(
            !state.bind(&refused, &p, true),
            "typed binding must be refused"
        );
        drop(title);
        host.execute(&[&lists[0], &lists[1]]);
        for n in 0..2 {
            let stride = strides[n] as usize;
            let mut expected = [if n == 0 { 17 } else { 23 }; WORDS];
            let [read, store, atomic] = offsets.map(|offset| offset as usize);
            let [read_index, store_index, atomic_index] = indices.map(|i| i as usize);
            let fits = |index, offset, bytes| index < 4 && offset + bytes <= stride;
            if fits(store_index, store, 4 * width) {
                let to = ((4 + store_index) * stride + store) / 4;
                let values = if fits(read_index, read, 4 * width) {
                    let from = ((4 + read_index) * stride + read) / 4;
                    &inputs[n].0[from..from + width]
                } else {
                    &[0; 4][..width]
                };
                expected[to..to + width].copy_from_slice(values);
            }
            if fits(atomic_index, atomic, 4) {
                expected[((4 + atomic_index) * stride + atomic) / 4] += 11;
            }
            assert_eq!(
                *outputs[n].0, expected,
                "case {case} descriptor revision {n}"
            );
        }
    }
    // SAFETY: this cookie identifies the registered callback above.
    unsafe { validation.UnregisterMessageCallback(cookie).unwrap() };
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
