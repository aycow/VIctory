# Bounded mesh projection control

This source-built fixture dispatches 64 mesh groups and emits one triangle
per group. Its four shaders differ only in clip-space W and the matching
Z needed to keep normalized depth at 0.5. Each dispatch uses the same
32 × 32 target, clear value, and constant pixel shader. A pipeline
statistics query encloses the dispatch. The raster probe counts the
distinct covered target pixels after all 64 triangles have drawn.

## Reproduction

Run with Windows Graphics Tools, a 1.620.x Agility SDK, and the Windows
SDK DXIL validator.

```powershell
$env:VICTORY_DXIL = 'C:\path\to\dxil.dll'
$env:VICTORY_MESH_MASK_TEST_SDK = 'C:\path\to\Agility\build\native\bin\x64'
$env:VICTORY_MESH_MASK_TEST_ADAPTER = 'amd' # or warp, nvidia
cargo test --locked -p victory-shader --lib emit::mesh_projection_tests::clip_space_w_changes_pixel_work_at_fixed_mesh_output_counts -- --exact --ignored --nocapture --test-threads=1
```

## Observed counters

Both AMD Radeon Graphics (driver 32.0.22042.1) and Agility WARP passed
DXIL validation, full GPU-based validation, and the raster assertions.
Both reported 64 `CInvocations` in every case.

| Clip-space W | Covered pixels of 1,024 | AMD CPrimitives | AMD PSInvocations | WARP CPrimitives | WARP PSInvocations |
|---:|---:|---:|---:|---:|---:|
| 1 | 276 | 64 | 17,664 | 64 | 19,968 |
| 0.5 | 888 | 64 | 56,832 | 192 | 66,560 |
| 0.125 | 1,024 | 64 | 65,536 | 128 | 69,632 |
| -1 | 0 | 0 | 0 | 0 | 0 |

Projection alone can multiply pixel work even when authored mesh output
counts are fixed. Both devices agree on coverage, while the reported
pipeline counters vary by adapter. Keep authored primitive counts,
post-clipping counters and unique pixel coverage distinct when reading
these results. This fixture measures counters and raster output. It has
no title inputs or timing attribution. The query lives in the test host.
[#1723 production diagnostics](https://github.com/Marvisak/VIctory/pull/1723)
