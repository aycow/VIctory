use super::*;
use victory_shader::native::{ComputeIo, StageIo};
use victory_shader::{
    Container, HwStage, NativeProgram, RootParam, emit, io, ir, resource, structure,
};

struct TitleBuffer(Box<[u32]>);

impl TitleBuffer {
    fn new(bytes: u64) -> Self {
        let title = Self(vec![17; (bytes / 4) as usize].into_boxed_slice());
        let address = title.address();
        crate::shadow::register(address as usize, address, bytes);
        title
    }

    fn address(&self) -> u64 {
        self.0.as_ptr() as u64
    }
}

impl Drop for TitleBuffer {
    fn drop(&mut self) {
        let address = self.address();
        crate::shadow::unregister(address as usize, address);
    }
}

struct Heap(This);

impl Heap {
    fn new(sampler: bool) -> Self {
        Self(crate::descriptor::test_heap(sampler).0)
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        crate::object::slot::<extern "system" fn(This) -> u32>(self.0, 16)(self.0);
    }
}

fn pipeline(offset: u32) -> HostPipeline {
    let words = [
        0xf40c_0400u32,
        0xfa00_0008, // s_load_dwordx8 s[16:23], s[0:1], 8
        0xf404_0200,
        0xfa00_0000, // s_load_dwordx2 s[8:9], s[0:1], 0; original root address
        0xbe8a_03c1, // s_mov_b32 s10, -1
        0xbe8b_0380, // s_mov_b32 s11, 0
        0x7e00_029d, // v_mov_b32 v0, 29
        0x7e02_02ff,
        offset, // v_mov_b32 v1, offset
        0xe070_1000,
        0x8002_0001, // buffer_store_dword v0, v1, s[8:11], 0 offen
        0xbf81_0000,
    ];
    let root = |root, sgprs, user_data, slot| RootParam {
        root,
        sgprs,
        user_data,
        slot,
        table_offset_dwords: 0,
        base_register: 0,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 2,
        layout: vec![
            root(RootType::Descriptor, 2, 16, 3),
            root(RootType::Constants, 8, 18, 4),
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Descriptor,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 16,
            slot: 3,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 4 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
    };
    assert!(native.spills_root_args());
    let p = ir::build_with(
        &native.code,
        native.wave64,
        native.vgprs,
        native.spills_root_args(),
    )
    .unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let c = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let mut shader = emit::emit(&c, &native, &p, &st, &io, &res).unwrap();
    assert_eq!(shader.root_uavs, [3]);
    emit::validator::Validator::load(emit::validator::Validator::default_path().as_deref())
        .expect("DXIL validator")
        .validate(&mut shader.dxil)
        .unwrap();
    HostPipeline::new(
        &[crate::pipeline::test_compute_shader(shader, native.used)],
        None,
    )
    .unwrap()
}

fn dispatch(
    host: &crate::host::Host,
    views: &Heap,
    samplers: &Heap,
    root: u64,
    p: &HostPipeline,
    after_bind: impl FnOnce(&HostList),
) {
    let mut state = State::default();
    state.set_heaps(&[views.0, samplers.0]);
    state.set_view(true, 3, root);
    state.set_constants(true, 4, &[100; 8], 0);
    let list = host.new_list().unwrap();
    assert!(state.bind(&list, p, true));
    // SAFETY: an open list with a bound compute pipeline and live descriptors.
    unsafe { list.list.Dispatch(1, 1, 1) };
    after_bind(&list);
    list.close();
    host.execute(&[&list]);
}

#[test]
#[ignore = "requires Agility SDK 1.620.2 and explicit adapter"]
fn spilled_root_uav_bounds_and_null_binding_are_observable() {
    assert!(std::env::var_os("VICTORY_TEST_SPILLED_UAV_ADAPTER").is_some());
    let host = crate::host::host().expect("adapter");
    let queue: ID3D12InfoQueue = host.device.cast().expect("debug layer");
    let views = Heap::new(false);
    let samplers = Heap::new(true);
    for (bytes, offset, written) in [
        (20u64, 0u32, true),
        (24, 8, false),
        (ROOT_UAV_BYTES + 32, ROOT_UAV_BYTES as u32 - 4, true),
        (ROOT_UAV_BYTES + 32, ROOT_UAV_BYTES as u32, false),
    ] {
        let buffer = TitleBuffer::new(bytes);
        let p = pipeline(offset);
        dispatch(host, &views, &samplers, buffer.address() + 16, &p, |_| {});
        let mut expected = vec![17; (bytes / 4) as usize];
        if written {
            expected[4 + offset as usize / 4] = 29;
        }
        assert_eq!(
            buffer.0.as_ref(),
            &expected,
            "bytes={bytes} offset={offset}"
        );
    }

    // A misaligned root UAV is a null descriptor. It cannot write, so later
    // in the same list ordinary refreshes must still see title writes.
    let buffer = TitleBuffer::new(256);
    let p = pipeline(0);
    let word = buffer.address() + 64;
    let shadow = crate::shadow::find(buffer.address(), 256).unwrap();
    dispatch(host, &views, &samplers, buffer.address() + 4, &p, |list| {
        list.defer(move || {
            // SAFETY: the live fixture's own title buffer, before GPU execution.
            unsafe { (word as *mut u32).write(77) };
        });
        shadow.refresh(list, word, 4);
    });
    // SAFETY: read the dword inside the live, persistently mapped shadow after the fence.
    let after = unsafe {
        let mut ptr = std::ptr::null_mut();
        shadow.buffer.Map(0, None, Some(&mut ptr)).unwrap();
        let value = ptr.cast::<u32>().add(16).read();
        shadow.buffer.Unmap(0, None);
        value
    };
    assert_eq!(buffer.0[1], 17, "the unaligned UAV must remain null");
    assert_eq!(buffer.0[16], 77, "the title's CPU write must persist");
    // SAFETY: both queries target the live device's debug queue and aligned storage.
    unsafe {
        for i in 0..queue.GetNumStoredMessages() {
            let mut bytes = 0;
            queue.GetMessage(i, None, &mut bytes).unwrap();
            let mut storage = vec![0u64; bytes.div_ceil(8)];
            let msg = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
            queue.GetMessage(i, Some(msg), &mut bytes).unwrap();
            let msg = &*msg;
            assert!(
                msg.Severity.0 > D3D12_MESSAGE_SEVERITY_ERROR.0,
                "{}",
                std::ffi::CStr::from_ptr(msg.pDescription.cast()).to_string_lossy()
            );
        }
    }
    assert_eq!(
        after, 77,
        "a null root UAV must not exclude a later refresh"
    );
}
