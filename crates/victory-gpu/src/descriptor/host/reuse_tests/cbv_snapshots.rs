//! An original shader reading two words of a rebuilt CBV. The production
//! table builder and submission loop supply each list's snapshot.

use super::*;
use crate::descriptor::{
    HEAP_IIDS, HeapDesc, Placement, create_cbv, create_heap, create_placed_srv, heap, release,
};
use crate::draw::State;
use crate::host::{HostList, PREPARING, test_install_device};
use crate::object::{S_OK, This, guid};
use victory_shader::{ResourceType, RootType, UsedResource};

const HLSL: &str = r#"
#define RS "RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED), RootConstants(num32BitConstants=3, b0), UAV(u0)"
cbuffer Bindings : register(b0) { uint slot, index, tail; };
RWByteAddressBuffer output : register(u0);
struct Values { uint4 rows[16]; };
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    ConstantBuffer<Values> values = ResourceDescriptorHeap[slot];
    uint word = 0;
    if (tail != 0) {
        ByteAddressBuffer input = ResourceDescriptorHeap[slot + tail];
        word = input.Load(0);
    }
    output.Store3(index * 12, uint3(values.rows[0].x, values.rows[15].w, word));
}
"#;

#[repr(align(16))]
struct Payload([u32; 64]);

impl Payload {
    fn from_pair(pair: [u32; 2]) -> Self {
        let mut words = [0; 64];
        words[0] = pair[0];
        words[63] = pair[1];
        Self(words)
    }
}

struct TitleBuffer(This, *mut Payload);

impl TitleBuffer {
    fn new(pair: [u32; 2]) -> Self {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size_of::<Payload>() as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let iid = guid(
            0x696442be, 0xa72e, 0x4059, [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        let mut title = std::ptr::null();
        assert_eq!(
            crate::resource::create_committed(
                std::ptr::null(),
                (&raw const heap).cast(),
                0,
                (&raw const desc).cast(),
                0,
                std::ptr::null(),
                &iid,
                &mut title,
            ),
            S_OK
        );
        let (data, size) = crate::resource::buffer_bytes(title).unwrap();
        assert_eq!(size, size_of::<Payload>() as u64);
        let data = data.cast::<Payload>();
        // SAFETY: the title buffer is newly allocated with room for Payload.
        unsafe { data.write(Payload::from_pair(pair)) };
        Self(title, data)
    }
}

impl Drop for TitleBuffer {
    fn drop(&mut self) {
        use crate::object::slot;
        slot::<extern "system" fn(This) -> u32>(self.0, 0x10)(self.0);
    }
}

struct PlacedInput(Box<Payload>);

impl PlacedInput {
    fn new(tail_slot: u64) -> Self {
        let input = Self(Box::new(Payload::from_pair([881, 0])));
        let address = input.0.0.as_ptr() as u64;
        crate::shadow::register(address as usize, address, 256);
        let mut resource = [0; 14];
        resource[0] = 1;
        resource[4] = 256;
        let mut view = [0; 16];
        view[0] = DXGI_FORMAT_R32_TYPELESS.0 as u32;
        view[1] = D3D12_SRV_DIMENSION_BUFFER.0 as u32;
        view[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        view[6] = 64;
        view[8] = D3D12_BUFFER_SRV_FLAG_RAW.0 as u32;
        view[10] = address as u32;
        view[11] = (address >> 32) as u32;
        create_placed_srv(std::ptr::null(), &resource, &view, tail_slot);
        input
    }
}

impl Drop for PlacedInput {
    fn drop(&mut self) {
        let address = self.0.0.as_ptr() as u64;
        crate::shadow::unregister(address as usize, address);
    }
}

struct TitleHeap(This, u32);

impl TitleHeap {
    fn new(width: u32) -> Self {
        #[repr(C)]
        struct Extended(HeapDesc, Placement);
        let desc = Extended(
            HeapDesc {
                kind: 0,
                count: width,
                flags: 1,
                nodes: 0,
            },
            Placement {
                address: 0,
                size: 0,
            },
        );
        let mut object = std::ptr::null();
        assert_eq!(
            create_heap(std::ptr::null(), &desc.0, &HEAP_IIDS[0], &mut object),
            S_OK
        );
        Self(object, width)
    }

    fn cbv_slot(&self, state: &mut State, list: &HostList) -> u32 {
        state.set_heaps(&[self.0]);
        state.set_table(true, 0, heap(self.0).start);
        let cbv = UsedResource {
            kind: ResourceType::Cbv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        };
        let srv = UsedResource {
            kind: ResourceType::Srv,
            table_offset_dwords: (self.1 - 1) * 8,
            dwords: 8,
            ..cbv
        };
        let used = [cbv, srv];
        let slots = state.test_compute_tables(list, &used[..if self.1 == 1 { 1 } else { 2 }]);
        let [(0, offset)] = slots.as_slice() else {
            panic!("no rebuilt CBV table")
        };
        offset / 32
    }
}

impl Drop for TitleHeap {
    fn drop(&mut self) {
        assert_eq!(release(self.0), 0);
    }
}

#[test]
#[ignore = "requires a named D3D12 adapter, DXC and Agility SDK 1.620.x; run alone"]
fn repeated_cbv_records_require_separate_submit_time_snapshots() {
    let (device, name) = device(true);
    let info: ID3D12InfoQueue = device.cast().unwrap();
    let host = test_install_device(device.clone());
    let code = shader(HLSL);
    let (root, pipeline) = compute_pipeline(&device, &code);
    let size = size_of::<Payload>();
    assert_eq!(size, 256);
    let title = TitleBuffer::new([11, 12]);
    let replacement = TitleBuffer::new([13, 14]);
    let title_ptr = title.1.cast::<u8>();
    let replacement_ptr = replacement.1.cast::<u8>();
    let width =
        std::env::var("VICTORY_CBV_SNAPSHOT_WIDTH").map_or(1, |width| width.parse().unwrap());
    assert!([1, 16, 178, 256].contains(&width));
    let descriptors = TitleHeap::new(width);
    let tail = width - 1;
    let _input =
        (tail != 0).then(|| PlacedInput::new(heap(descriptors.0).start + u64::from(tail) * 32));
    let source = title_ptr as u64;
    create_cbv(
        std::ptr::null(),
        &[source as u32, (source >> 32) as u32, size as u32, 0],
        heap(descriptors.0).start,
    );
    let heap = heap(descriptors.0).host().unwrap();

    let first = host.new_list().unwrap();
    let second = host.new_list().unwrap();
    let readback = host.new_list().unwrap();
    let single = host.new_list().unwrap();
    let mut first_state = State::default();
    let mut second_state = State::default();
    let first_slot = descriptors.cbv_slot(&mut first_state, &first);
    second.defer_copy(title_ptr, replacement_ptr, size);
    let second_slot = descriptors.cbv_slot(&mut second_state, &second);
    assert_ne!(first_slot, second_slot, "separate native table slots");

    let mut single_state = State::default();
    let early_slot = descriptors.cbv_slot(&mut single_state, &single);
    single.defer_copy(title_ptr, replacement_ptr, size);
    // Also exercise the ordered callback path without changing title bytes.
    single.defer((0, 0), (0, 0), || {});
    let late_slot = descriptors.cbv_slot(&mut single_state, &single);

    let ordered = host.new_title_list().unwrap();
    let mut ordered_state = State::default();
    let ordered_first = descriptors.cbv_slot(&mut ordered_state, &ordered);

    const RESULT_BYTES: u64 = 7 * 12;
    let output = resource(
        &device,
        D3D12_HEAP_TYPE_DEFAULT,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        &buf(RESULT_BYTES, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
    );
    let back = resource(
        &device,
        D3D12_HEAP_TYPE_READBACK,
        D3D12_RESOURCE_STATE_COPY_DEST,
        &buf(RESULT_BYTES, D3D12_RESOURCE_FLAG_NONE),
    );

    // SAFETY: the pipeline, open lists, output and descriptor ranges all
    // remain live across recording and every fenced submission below.
    unsafe {
        let record = |list: &HostList, table_slot: u32, index: u32| {
            list.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
            list.list.SetComputeRootSignature(&root);
            list.list.SetPipelineState(&pipeline);
            let data = [table_slot, index, tail];
            list.list
                .SetComputeRoot32BitConstants(0, 3, data.as_ptr().cast(), 0);
            list.list
                .SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
            list.list.Dispatch(1, 1, 1);
        };
        record(&first, first_slot, 0);
        record(&second, second_slot, 1);
        // Model sharing by descriptor-record identity while preserving both
        // real fills. This control reads list one's snapshot in list two.
        record(&second, first_slot, 2);
        first.close();
        second.close();
        readback.list.ResourceBarrier(&[crate::host::transition(
            &output,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        readback
            .list
            .CopyBufferRegion(&back, 0, &output, 0, RESULT_BYTES);
        readback.list.ResourceBarrier(&[crate::host::transition(
            &output,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
        readback.close();
        record(&single, early_slot, 3);
        record(&single, late_slot, 4);
        single.close();

        record(&ordered, ordered_first, 5);
        ordered.buffer_copy(
            title.0,
            title_ptr as usize,
            replacement.0,
            replacement_ptr as usize,
            size,
        );
        let (continuation, fresh) = ordered.recording().expect("copy continuation");
        assert!(fresh);
        let ordered_second = descriptors.cbv_slot(&mut ordered_state, &continuation);
        record(&continuation, ordered_second, 6);
        ordered.close();

        for (round, a, b) in [
            ("distinct", [101, 102], [201, 202]),
            ("equal", [301, 302], [301, 302]),
            ("replayed distinct", [401, 402], [501, 502]),
        ] {
            // Rewrite both sources after recording, including when the very
            // same closed lists are submitted again.
            title_ptr.cast::<Payload>().write(Payload::from_pair(a));
            replacement_ptr
                .cast::<Payload>()
                .write(Payload::from_pair(b));
            let before = PREPARING[1].load(std::sync::atomic::Ordering::Relaxed);
            host.execute(&[&first, &second, &readback]);
            host.drain();
            let copied = PREPARING[1].load(std::sync::atomic::Ordering::Relaxed) - before;
            if tail == 0 {
                assert_eq!(copied, (3 * size) as u64);
            }
            // Same CBV record, same list, and two different submit-time contents.
            title_ptr.cast::<Payload>().write(Payload::from_pair(a));
            host.execute(&[&single, &readback]);
            host.drain();
            // Same State and CBV record across a native buffer copy and
            // the CPU handoff that makes its destination safe to stage.
            title_ptr.cast::<Payload>().write(Payload::from_pair(a));
            host.execute(&[&ordered, &readback]);
            host.drain();
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data)).unwrap();
            let results = std::slice::from_raw_parts(data.cast::<[u32; 3]>(), 7);
            let raw = if tail == 0 { 0 } else { 881 };
            let expected = [a, b, a, a, b, a, b].map(|[lo, hi]| [lo, hi, raw]);
            assert_eq!(results, &expected);
            assert_eq!(results[2] == results[1], a == b);
            eprintln!(
                "{name} {round}: width={width} total reserved slots={} ordered versions={} bytes; ordered copy={size} bytes; native={results:?}",
                6 * width,
                2 * size
            );
            back.Unmap(0, Some(&D3D12_RANGE::default()));
        }
        device.GetDeviceRemovedReason().unwrap();
    }
    check_debug_messages(&info);
}
