# Descriptor allocation demand

Branch `gpu/descriptor-allocation-demand` is a diagnostics follow-up to #562 at `b82f29c`. It measures rebuilt dynamic descriptor tables through the draw binding path. It keeps the same list-local scratch, pool size, native materializer, and range lifetimes.

## What the census measures

- `outcome` reports tables and slots for new and reused shared ranges, new and reused list-local ranges, and requests that did not get a range. `NoRange` measures a rebuilt table allocation failure. On this base the surrounding binding path still follows its existing admission behavior.
- `widths` divides requests, new reservations and failures into 1, 2–4, 5–16, 17–64, 65–256, 257–1,024, 1,025–4,096, and wider slots.
- `descriptors` breaks down inspected slots by resource and access type, including unknown, empty and stale records. `shader_cbv` counts tables with translated CBV use, and `recorded_cbv` counts inspected tables with a live CBV record. The CBV composition flag is their union.
- `resolved_textures` counts requests whose records pass #562's texture resolver. `SharedNew` and `SharedReuse` count successful native materialization or a live cache hit. Native format checks and allocation availability still apply after resolution.
- `exact_cross_list` counts a repeated sequence of descriptor record words after the sequence has appeared on a different list recording in the same native heap and report window. `exact_cross_list_reserved` restricts that count to requests that reserved a new range. This measures possible descriptor metadata duplication. Concurrent range lifetimes, native shadow identity and CBV upload byte equality require separate evidence.
- `generation_changed` counts changes in the generation of the same registered view index at a matching offset in the same source table and width. `Stale` counts inspected records that no longer resolve to their recorded generation. Neither count identifies remapping of a placed view or a change to the host shadow selected for a live record.

The collector scans up to 4,096 slots per request and marks wider or invalid spans uninspected. Each exact history keeps at most 256 keys and 65,536 tracked slot records per report. Aggregate demand continues when histories are full, with explicit untracked counters. A report window ends at presents 1, 2, 4, 8 and every 16 after that, or on the first allocation failure in the process. Trace lines include aggregate counts and a present index.

## Synthetic validation

```powershell
cargo test --locked -p victory-gpu --features diagnostics descriptor::demand -- --nocapture
```

The fixture copies original mixed CBV/placed descriptors at different table addresses, verifies exact duplicate accounting across list and heap identities, and changes a generation to produce a stale record. It checks invalid spans and the history budgets. The existing capped-pool fixture now checks actual native cache-hit status and passes on AMD Radeon Graphics, NVIDIA RTX 5070 Laptop GPU, and WARP with the debug layer and GPU-based validation. It exercises the native allocator/materializer and GPU readback. The CPU regression exercises the diagnostics registry scan and aggregation.

```powershell
$env:VICTORY_TABLE_TEST_AGILITY = 'C:/path/to/Agility/build/native/bin/x64'
$env:VICTORY_TABLE_TEST_DXC = 'C:/path/to/dxc.exe'
$env:VICTORY_TABLE_TEST_ADAPTER = 'amd' # repeat with nvidia and warp
cargo test --locked -p victory-gpu --features diagnostics capped_pool_texture_table_reuse -- --ignored --nocapture
```

## Authorized tester capture

Build with `cargo build --locked --profile diag -p victory --features diagnostics`. Set `VICTORY_DESCRIPTOR_DEMAND=1`, follow the same warm-cache menu-to-world route used for the baseline, and retain both lines matching `rebuilt descriptor demand`. Share only those aggregates, the route phase, the commit, GPU/driver and whether the run reached the world or encountered pool exhaustion. For frame-time A/B, use a separate pass with descriptor demand disabled so scanning and aggregation are outside the measured workflow.

## Reuse audit

| Candidate | Required invariant and current evidence |
| --- | --- |
| Constant-buffer tables | The title bytes are copied into list-owned scratch at submission. Identical descriptor words still allow different bytes or scratch GPU addresses. Reuse would need a proven version of immutable upload storage with fence-delayed ownership. |
| Buffer SRV/UAV | `shadow::find` selects a containing title allocation, and its concrete native buffer and adjusted element offset can change after unregister/re-register or alias selection. A safe key needs the resolved shadow generation, native resource identity, adjusted view geometry, and applicable counter identity. Each list still needs refresh and writeback registration. Descriptor-kind and stride semantics are under #193. |
| Mixed-resource tables | Every slot must satisfy its own resolution and lifetime proof, including any CBV scratch. The aggregate records can show which mixed cases deserve a targeted synthetic fixture. |
| Placed views | Guest addresses and descriptor words can stay stable across alias replacement. Sharing needs the concrete backing and alias generation captured with the snapshot and retained through the fence. #643 already addresses live texture aliases and canonicalization. |

PRs #251, #336 and #420 cover fragmentation, allocation refusal and table width respectively. The direct bindless path and one-slot root SRV allocation are out of this census. Async fence ownership is a separate compatibility obligation for #595 and #32. The investigation retains the current sharing eligibility until title measurements and native lifetime regressions justify an extension.

Related contributions include [#562](https://github.com/Marvisak/VIctory/pull/562), [#193](https://github.com/Marvisak/VIctory/pull/193), [#643](https://github.com/Marvisak/VIctory/pull/643), [#251](https://github.com/Marvisak/VIctory/pull/251), [#336](https://github.com/Marvisak/VIctory/pull/336), [#420](https://github.com/Marvisak/VIctory/pull/420), [#595](https://github.com/Marvisak/VIctory/pull/595), [#32](https://github.com/Marvisak/VIctory/pull/32)
