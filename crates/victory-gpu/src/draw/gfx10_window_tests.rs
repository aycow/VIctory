use super::{HostPipeline, State};
use crate::descriptor::{self, HeapDesc};
use crate::object::{S_OK, This, guid, slot};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use victory_shader::native::{ComputeIo, StageIo};
use victory_shader::{
    Container, HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource, emit, io,
    ir, resource, structure,
};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{Interface, PCSTR};

const GUEST: u64 = 0x10_ffff_fff0;
const GUEST_OUTPUT: u64 = 0x20_ffff_fff0;
const WORDS: usize = 16;
const LEN: u64 = (WORDS * 4) as u64;
const READ: u64 = GUEST + 16;
const SENTINEL: u32 = 0xceda_002a;

#[derive(Clone, Copy)]
struct Window {
    guest: u64,
    native: u64,
    bytes: u64,
}

impl Window {
    fn span(self, guest: u64, width: u64) -> Option<u64> {
        // These root SRVs are raw views and require a 16-byte base; the
        // shader will issue two 4-byte reads at offsets zero and four.
        let offset = guest.checked_sub(self.guest)?;
        guest.checked_add(width)?;
        if width == 0 || !offset.is_multiple_of(16) || offset.checked_add(width)? > self.bytes {
            return None;
        }
        let native = self.native.checked_add(offset)?;
        native.checked_add(width)?;
        native.is_multiple_of(16).then_some(native)
    }
}

#[test]
fn root_window_rejects_partial_or_unaligned_spans() {
    let window = Window {
        guest: GUEST,
        native: 0x8000,
        bytes: LEN,
    };
    assert_eq!(window.span(READ, 8), Some(0x8010));
    assert_eq!(window.span(GUEST + LEN - 16, 16), Some(0x8030));
    assert_eq!(window.span(GUEST + LEN - 16, 17), None);
    assert_eq!(window.span(GUEST + LEN, 4), None);
    assert_eq!(window.span(GUEST - 16, 4), None);
    assert_eq!(window.span(READ + 4, 4), None);
    assert_eq!(window.span(READ, 0), None);
    assert_eq!(window.span(READ, u64::MAX), None);
    assert_eq!(
        Window {
            guest: u64::MAX - 15,
            ..window
        }
        .span(u64::MAX - 15, 16),
        None
    );
    assert_eq!(
        Window {
            native: u64::MAX - 15,
            ..window
        }
        .span(READ, 8),
        None
    );
}

#[repr(align(16))]
struct Aligned([u32; WORDS]);

struct TitleWords(Box<Aligned>);

impl TitleWords {
    fn new(words: [u32; WORDS]) -> Self {
        let memory = Self(Box::new(Aligned(words)));
        let address = memory.address();
        crate::shadow::register(address as usize, address, LEN);
        memory
    }

    fn address(&self) -> u64 {
        self.0.0.as_ptr() as u64
    }
}

impl Drop for TitleWords {
    fn drop(&mut self) {
        crate::shadow::unregister(self.address() as usize, self.address());
    }
}

struct TitleHeap(This);

impl TitleHeap {
    fn new(sampler: bool) -> Self {
        #[repr(C)]
        struct Extended {
            words: [u32; 4],
            address: u64,
            size: u64,
        }
        let desc = Extended {
            words: [if sampler { 1 } else { 0 }, 4, 1, 0],
            address: 0,
            size: 0,
        };
        let iid = guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        let mut heap = std::ptr::null();
        assert_eq!(
            descriptor::create_heap(
                std::ptr::null(),
                (&raw const desc).cast::<HeapDesc>(),
                &iid,
                &mut heap
            ),
            S_OK
        );
        Self(heap)
    }
}

impl Drop for TitleHeap {
    fn drop(&mut self) {
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(self.0, 0x10)(self.0),
            0
        );
    }
}

fn translated() -> (emit::HostShader, Vec<UsedResource>) {
    let words = [
        0x7e00_0280u32, // v_mov_b32 v0, 0
        0xbe82_0390,    // s_mov_b32 s2, 16
        0xbe83_0380,    // s_mov_b32 s3, 0
        0xbe86_0390,    // s_mov_b32 s6, 16
        0xbe87_0380,    // s_mov_b32 s7, 0
        0xdc30_8000,
        0x0200_0000, // global_load_dword v2, v0, s[0:1]
        0xf420_0200,
        0xfa00_0004, // s_buffer_load_dword s8, s[0:3], 4
        0xbf8c_0070, // s_waitcnt vmcnt(0) lgkmcnt(0)
        0x7e06_0208, // v_mov_b32 v3, s8
        0xe070_0000,
        0x8001_0200, // buffer_store_dword v2, off, s[4:7], 0
        0xe0c8_0004,
        0x8001_0300, // buffer_atomic_add v3, off, s[4:7], 0 offset:4
        0xe070_0008,
        0x8001_0300, // buffer_store_dword v3, off, s[4:7], 0 offset:8
        0xbf81_0000, // s_endpgm
    ];
    let mut native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 6,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 6 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
    };
    for (slot, user_data, kind) in [(0, 0, ResourceType::Srv), (1, 4, ResourceType::Uav)] {
        native.layout.push(RootParam {
            root: RootType::Descriptor,
            table_offset_dwords: 0,
            sgprs: 2,
            base_register: 0,
            user_data,
            slot,
        });
        native.used.push(UsedResource {
            kind,
            root: RootType::Descriptor,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data,
            slot,
        });
    }
    let program = ir::build(&native.code, false, native.vgprs).unwrap();
    for opcode in [
        "global_load_dword",
        "s_buffer_load_dword",
        "buffer_store_dword",
        "buffer_atomic_add",
    ] {
        assert!(
            program
                .blocks
                .iter()
                .flat_map(|block| &block.insts)
                .any(|inst| inst.name() == opcode),
            "{opcode}"
        );
    }
    let structured = structure::structure(&program).unwrap();
    let resources =
        resource::track(&program, &native.layout, native.stage, Some(&structured)).unwrap();
    let stage = io::raise(&program, &native).unwrap();
    let container = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    let shader = emit::emit(
        &container,
        &native,
        &program,
        &structured,
        &stage,
        &resources,
    )
    .unwrap();
    assert_eq!(shader.root_srvs, [0]);
    assert_eq!(shader.root_uavs, [1]);
    (shader, native.used)
}

#[test]
fn root_window_gfx10_instructions_translate() {
    translated();
}

unsafe extern "system" fn validation(
    _: D3D12_MESSAGE_CATEGORY,
    severity: D3D12_MESSAGE_SEVERITY,
    _: D3D12_MESSAGE_ID,
    text: PCSTR,
    context: *mut c_void,
) {
    if severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
        // SAFETY: the callback is registered with a process-lifetime counter and a valid message.
        let (errors, text) = unsafe {
            (
                &*context.cast::<AtomicUsize>(),
                text.to_string().unwrap_or_default(),
            )
        };
        errors.fetch_add(1, Ordering::Relaxed);
        eprintln!("{text}");
    }
}

#[test]
#[ignore = "requires Agility SDK 1.620.x, DXIL validator and an explicit D3D12 adapter"]
fn emitted_gfx10_load_store_and_atomic_through_checked_windows() {
    assert!(std::env::var_os("VICTORY_SHARED_TEST_ADAPTER").is_some());
    assert!(std::env::var_os("VICTORY_SHARED_TEST_TIMING").is_none());
    let host = crate::host::host().expect("configured D3D12 test device");
    let info = host
        .device
        .cast::<ID3D12InfoQueue1>()
        .expect("active debug layer");
    static ERRORS: AtomicUsize = AtomicUsize::new(0);
    let mut cookie = 0;
    // SAFETY: the live queue holds a callback with a static context.
    unsafe {
        info.RegisterMessageCallback(
            Some(validation),
            D3D12_MESSAGE_CALLBACK_FLAG_NONE,
            std::ptr::from_ref(&ERRORS).cast_mut().cast(),
            &mut cookie,
        )
        .unwrap();
    }
    let (mut shader, used) = translated();
    emit::validator::Validator::load(emit::validator::Validator::default_path().as_deref())
        .expect("available DXIL validator")
        .validate(&mut shader.dxil)
        .expect("translated DXIL validates");
    let pipeline = HostPipeline::new(&[crate::pipeline::test_compute_shader(shader, used)], None)
        .expect("translated pipeline");
    let view = TitleHeap::new(false);
    let sampler = TitleHeap::new(true);
    let prefix = host.unified_buffer(256).expect("shared prefix");
    let shared = host.unified_buffer(LEN).expect("shared root SRV");
    assert_eq!(shared.offset, prefix.offset + 256);
    let title = TitleWords::new(std::array::from_fn(|i| 17 + i as u32 * 3));
    let shared_words: [u32; WORDS] = std::array::from_fn(|i| 101 + i as u32 * 5);
    let shadow = crate::shadow::find(title.address(), LEN).unwrap();
    assert_ne!(shared.gpu, title.address());
    assert_ne!(shared.gpu, shared.data as u64);
    assert_ne!(shared.gpu, shadow.gpu(title.address()));
    assert_ne!(GUEST, title.address());
    assert_ne!(GUEST, shared.gpu);
    let output_initial = std::array::from_fn(|i| if i == 5 { 11 } else { SENTINEL });
    for shared_input in [false, true] {
        let out = TitleWords::new(output_initial);
        let output = Window {
            guest: GUEST_OUTPUT,
            native: out.address(),
            bytes: LEN,
        };
        let write = GUEST_OUTPUT + 16;
        let title_output = output.span(write, 12).unwrap();
        let output_shadow = crate::shadow::find(out.address(), LEN).unwrap();
        let native_output = Window {
            native: output_shadow.gpu(out.address()),
            ..output
        }
        .span(write, 12)
        .unwrap();
        assert_ne!(native_output, title_output);
        assert_ne!(native_output, write);
        let source = if shared_input {
            shared.gpu
        } else {
            shadow.gpu(title.address())
        };
        let mapped = Window {
            guest: GUEST,
            native: source,
            bytes: LEN,
        };
        let native = mapped.span(READ, 8).unwrap();
        let title_view = Window {
            native: title.address(),
            ..mapped
        }
        .span(READ, 8)
        .unwrap();
        let list = host.new_list().unwrap();
        let mut state = State::default();
        state.set_heaps(&[view.0, sampler.0]);
        state.set_view(true, 0, title_view);
        state.set_view(true, 1, title_output);
        assert!(state.bind(&list, &pipeline, true));
        // SAFETY: selected live disjoint resources, and the window bounds
        // check covers both native SRV reads. CPU copies precede queue work.
        unsafe {
            if shared_input {
                std::ptr::copy_nonoverlapping(shared_words.as_ptr(), shared.data.cast(), WORDS);
                let [(_, param)] = pipeline.stages[0].root_srvs.as_slice() else {
                    panic!("one input root SRV")
                };
                list.list.SetComputeRootShaderResourceView(*param, native);
            }
            list.list.Dispatch(1, 1, 1);
        }
        list.close();
        host.execute(&[&list]);
        let source_words = if shared_input {
            &shared_words
        } else {
            &title.0.0
        };
        let mut expected = output_initial;
        expected[4] = source_words[4];
        expected[5] += source_words[5];
        expected[6] = source_words[5];
        assert_eq!(out.0.0, expected, "shared input={shared_input}");
        eprintln!(
            "emitted GFX10 shared={shared_input}: vector={} atomic={} scalar={} guards intact",
            out.0.0[4], out.0.0[5], out.0.0[6]
        );
    }
    // SAFETY: the callback and cookie are still ours and GPU work completed.
    unsafe {
        info.UnregisterMessageCallback(cookie).unwrap();
        host.device.GetDeviceRemovedReason().unwrap();
    }
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}
