# Rebuilt constant-buffer table snapshots

A rebuilt CBV table in #562 binds a native heap offset during recording.
The CBV points to that list's upload scratch. Each submission copies the
title bytes into the scratch when its ordered `Step::Fill` runs. A deferred
title copy can run between the fills for two lists using the same
descriptor record. Resubmitting a closed list also fills its scratch
again.
[PR #562](https://github.com/Marvisak/VIctory/pull/562)

The descriptor-demand collector can count the repeated record and the
new slots it reserves. It cannot establish that the upload bytes match.
Record-time source contents, equal bytes on a previous submission, and
matching guest address and size also fail to establish that invariant.
The existing table allocation and list-owned uploads are retained.

## Synthetic validation

Use the Agility SDK 1.620.x x64 DLL directory and a DXC with Shader Model
6.6 support. Run the ignored test alone because it installs its named
adapter before the normal host initializes.

```powershell
$env:VICTORY_TABLE_TEST_AGILITY = 'C:\path\to\Agility\build\native\bin\x64'
$env:VICTORY_TABLE_TEST_DXC = 'C:\path\to\dxc.exe'
foreach ($adapter in 'amd', 'nvidia', 'warp') {
    $env:VICTORY_TABLE_TEST_ADAPTER = $adapter
    cargo test --locked -p victory-gpu --features diagnostics repeated_cbv_records_require_separate_submit_time_snapshots -- --ignored --nocapture
    if ($LASTEXITCODE -ne 0) { throw "CBV test failed on $adapter" }
}
```

The fixture records two lists using the same title CBV through
`State::tables`. The second list's ordinary deferred copy replaces the
title data before its own fill. An original compute shader reads the
first and last dword of each 256-byte upload. A third dispatch deliberately
uses the first list's native slot in the second list to model sharing by
record identity. The fixture runs three batches using the same closed
recordings.

| Submission | List one | List two | Forced slot reuse in list two |
| --- | --- | --- | --- |
| Different bytes | 101, 102 | 201, 202 | 101, 102 |
| Equal bytes | 301, 302 | 301, 302 | 301, 302 |
| Different bytes on replay | 401, 402 | 501, 502 | 401, 402 |

These results matched on AMD Radeon Graphics, NVIDIA GeForce RTX 5070
Laptop GPU, and WARP with the D3D12 debug layer and GPU-based validation.
Each batch used two dynamic slots and processed 512 bytes of CBV fills
plus the separate 256-byte deferred title copy. These are synthetic
allocation and copy counts. In-game demand and performance remain
unmeasured.

## Requirements for reuse

- An immutable snapshot needs an exact byte key or a version issued by
  an owner that guarantees immutability, including the native view's
  extent and any readable alignment padding.
- Choosing a snapshot using submit-time contents needs a way to resolve
  the native slot after the ordered fill. Today the translated stage
  receives its heap offset as a recorded root constant. A deferred
  binding would need to choose the slot before GPU access while keeping
  each native descriptor immutable through the end of its recording
  and GPU use.
- #501's scratch probe changes where disjoint list uploads live. Ordered
  title fills still need their own versions. #644's generated stage-root
  data has a separate producer and binding path. #413's directly indexed
  views also use a separate preparation path.
  [PR #501](https://github.com/Marvisak/VIctory/pull/501),
  [PR #644](https://github.com/Marvisak/VIctory/pull/644),
  [PR #413](https://github.com/Marvisak/VIctory/pull/413)
- Both the upload and the descriptor range need suitable ownership for
  every referencing recording and submission. A mixed table also needs
  the resolved buffer shadow, native descriptor layout and applicable
  counter identity. Refresh and writeback registration still belong to
  the lists that use those buffers. Placed views need backing and mapping
  generation tracking.

The current rebuilt title CBV path exposes neither a sealed upload
version nor deferred native slot binding.
