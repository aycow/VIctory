# Directly indexed texture upload ownership

The #413 main merge `769d625` already schedules ordinary CPU-backed
texture SRVs as well as placed SRVs. It advances a heap-wide upload
timestamp while recording. The list that owns the native copy can be
discarded, or another list can read first. The resulting direct-heap
dispatch can legally sample stale contents.

In the original fixture, a native R32_UINT texture first receives
`0x12345678` and ends in shader-resource state. The CPU writes
`0x87654321` into its backing. Upload list A is recorded, then a second
list reads the same slot using `ResourceDescriptorHeap[0]`. The second
list is submitted first, with A either retained or discarded. AMD Radeon
Graphics, NVIDIA RTX 5070 Laptop, and WARP read the old value in all four
ordinary/placed and retained/discarded combinations. The debug layer and
full GPU-based validation were active. The stale reads produce no
graphics validation error.

## Reproduction

Use Windows Graphics Tools, Agility SDK 1.620.x and a Shader Model 6.6
DXC. Run each test and adapter in a fresh process.

```powershell
$env:VICTORY_BINDLESS_TEST_SDK = 'C:\path\to\Agility\build\native\bin\x64'
$env:VICTORY_BINDLESS_TEST_DXC = 'C:\path\to\dxc.exe'
$env:VICTORY_BINDLESS_TEST_ADAPTER = 'warp'  # or amd, nvidia
$test = 'descriptor::host::tests::texture_record_tests::'
cargo test --locked -p victory-gpu --features diagnostics --lib "${test}unsubmitted_first_recording_does_not_consume_a_bindless_texture_upload" -- --exact --ignored --nocapture --test-threads=1
```

On baseline `eca7f26`, the expected failure is the mismatch between the
old and new readback words. The producer control at `99c36da` additionally
orders a real native GPU copy before a consumer. Run
`a_recorded_bindless_upload_can_overwrite_a_later_submitted_gpu_producer`
with the same module prefix and flags.

The comparison branch combines `769d625` with #230 `8d81f61`.
Repeatedly run the stale test with `VICTORY_BINDLESS_TEST_REARM` unset to
exercise the existing #413 policy. Set it to `1` to explicitly give the
consuming test list its own request, leaving #230 to decide at submission
whether to copy. The producer comparison on this branch is
`a_bound_bindless_upload_preserves_an_earlier_submitted_gpu_producer`.

| Configuration | Result |
|---|---|
| #413 alone, producer runs after the upload is recorded | All six ordinary and placed cases returned the CPU upload `0x87654321`, overwriting the GPU value. |
| #413 alone, upload recorded after the producer runs | All six controls retained the GPU value `0xfeed1234`. |
| #413 + #230, either recording order | All 12 cases retained the GPU value `0xfeed1234`. |
| #413 + #230, fixture explicitly re-arms the consuming list | All 12 discard/reorder cases read the CPU value `0x87654321`. |
| #413 + #230, existing scheduler | The four AMD discard/reorder cases still read `0x12345678`. |

The full matrices used AMD Radeon Graphics, NVIDIA RTX 5070 Laptop, and WARP with full GPU-based validation.

## Integration boundary

#230 records changed texture copies in the submission prelude, with
ordered host-written state on the native texture. This supplies the
decision point for retries. A retry baked into another native list can
overwrite a GPU copy submitted between recording and execution.
Keeping a receipt for an outstanding bindless request until queue
execution also requires descriptor generation and host-resource identity
to remain valid. The explicit retry belongs to the fixture and leaves
the frame-budget and refresh scheduler unchanged.

#1273 fixes placed texture UAVs and #643 resolves compatible live
texture aliases. #1135 changes which subresources the CPU detiles. The
ordinary and placed inputs in this fixture each refer to one known native
texture. The fixture establishes behavior for the selected command
sequences, with no title-specific binding inference or performance
claim. [#413](https://github.com/Marvisak/VIctory/pull/413)
· [#230](https://github.com/Marvisak/VIctory/pull/230)
· [#1273](https://github.com/Marvisak/VIctory/pull/1273)
· [#643](https://github.com/Marvisak/VIctory/pull/643)
· [#1135](https://github.com/Marvisak/VIctory/pull/1135)
