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


## Same-list upload versions

The production table builder also has to distinguish the same CBV record before and after ordered title-memory writes in one recording. The cache now records the title-write version for rebuilt tables that contain a CBV. A new version receives a new descriptor range and staged upload. The list still owns its previous range because already-recorded draws reference it. Tables without CBVs keep their existing cache behavior.

Set `VICTORY_CBV_SNAPSHOT_WIDTH` to 1, 16, 178, or 256 and run the exact test in a fresh process per width and adapter. Width 1 is CBV-only. The other cases use an original mixed table with the CBV in the first slot and an actual raw placed SRV in the last. The shader checks both CBV words and reads the SRV value (881). The fixture replays the same recordings with different title bytes. The first two lists and the two versions in the single-list case hold four ranges, which occupy 4, 64, 712, or 1,024 slots across the respective widths.

All 12 adapter and width combinations passed on AMD Radeon Graphics, RTX 5070 Laptop, and WARP with full D3D12 GPU-based validation. The WARP width-178 mutation bypassing the version check reproduced a late result of [101, 102, 881] instead of [201, 202, 881]. This test exercises version correctness. Cross-list mixed-table sharing and the cost at title-scale pressure remain open.

## Ordered-copy continuations

In the async composition with #387, a title buffer copy ends the current
native command-list segment. A CBV cache lookup includes both the segment
identity and its local title-write version. The next segment gets its own
upload and descriptors even when its local counter starts at the same
value. Earlier native slots remain owned by their recording through its
retirement fence.

The same command above runs the additional ordered-copy case on this
branch. One draw state binds the same title CBV before and after the
production buffer-copy split. A native copy updates the title buffer and
the following CPU fill stages the new bytes. The replay reads
`[401, 402]` before the copy and `[501, 502]` after it. The mixed-table
cases keep the placed SRV value at 881. All 12 adapter and width
combinations passed with the debug layer and full GPU-based validation.
The fixture holds six ranges across its recordings, totaling 6, 96,
1,068, or 1,536 slots for the tested widths.
