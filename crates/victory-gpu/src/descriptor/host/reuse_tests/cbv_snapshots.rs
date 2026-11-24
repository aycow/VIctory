//! An original shader reading two words of a rebuilt CBV. The production
//! table builder and submission loop supply each list's snapshot.

use super::*;
use crate::descriptor::{HEAP_IIDS, HeapDesc, Placement, create_cbv, create_heap, heap, release};
use crate::draw::State;
use crate::host::{HostList, PREPARING, install_test_device};
use crate::object::{S_OK, This};
use victory_shader::{ResourceType, RootType, UsedResource};

const HLSL: &str = r#"
#define RS "RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED), RootConstants(num32BitConstants=2, b0), UAV(u0)"
cbuffer Bindings : register(b0) { uint slot, index; };
RWByteAddressBuffer output : register(u0);
struct Values { uint4 rows[16]; };
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    ConstantBuffer<Values> values = ResourceDescriptorHeap[slot];
    output.Store2(index * 8, uint2(values.rows[0].x, values.rows[15].w));
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

struct TitleHeap(This);

impl TitleHeap {
    fn new() -> Self {
        #[repr(C)]
        struct Extended(HeapDesc, Placement);
        let desc = Extended(
            HeapDesc {
                kind: 0,
                count: 8,
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
        Self(object)
    }

    fn cbv_slot(&self, state: &mut State, list: &HostList) -> u32 {
        state.set_heaps(&[self.0]);
        state.set_table(true, 0, heap(self.0).start);
        let used = [UsedResource {
            kind: ResourceType::Cbv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }];
        let slots = state.test_compute_tables(list, &used);
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
    let host = install_test_device(device.clone());
    let code = shader(HLSL);
    let (root, pipeline) = compute_pipeline(&device, &code);
    let size = size_of::<Payload>();
    assert_eq!(size, 256);
    let mut title = Box::new(Payload::from_pair([11, 12]));
    let mut replacement = Box::new(Payload::from_pair([13, 14]));
    let title_ptr = (&raw mut title.0).cast::<u8>();
    let replacement_ptr = (&raw mut replacement.0).cast::<u8>();
    let descriptors = TitleHeap::new();
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
    let mut first_state = State::default();
    let mut second_state = State::default();
    let first_slot = descriptors.cbv_slot(&mut first_state, &first);
    second.defer_copy(title_ptr, replacement_ptr, size);
    let second_slot = descriptors.cbv_slot(&mut second_state, &second);
    assert_ne!(first_slot, second_slot, "separate native table slots");

    const RESULT_BYTES: u64 = 3 * 8;
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
            let data = [table_slot, index];
            list.list
                .SetComputeRoot32BitConstants(0, 2, data.as_ptr().cast(), 0);
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
            let copied = PREPARING[1].load(std::sync::atomic::Ordering::Relaxed) - before;
            assert_eq!(copied, (3 * size) as u64);
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data)).unwrap();
            let results = std::slice::from_raw_parts(data.cast::<[u32; 2]>(), 3);
            assert_eq!(results, &[a, b, a]);
            assert_eq!(results[2] == results[1], a == b);
            eprintln!(
                "{name} {round}: identical CBV records=2 slots=2 fills={} bytes; ordered copy={size} bytes; native={results:?}",
                2 * size
            );
            back.Unmap(0, Some(&D3D12_RANGE::default()));
        }
        device.GetDeviceRemovedReason().unwrap();
    }
    check_debug_messages(&info);
}
