# Placed depth to indirect mesh

The diagnostic fixture uses original shaders and a pagefile-backed section
with two distinct Windows views. It writes a 128×72 R32_FLOAT depth pattern
through the sampled view, submits a 64×36 max reduction, and records a
culling dispatch that writes one native `D3D12_DISPATCH_MESH_ARGUMENTS`.
It then submits `ExecuteIndirect` and reads back a 16×16 color target.
For the second round it writes the opposite depth pattern through the
sibling CPU view. The shader reads both rounds from the same placed SRV
with layout `0x100`.

## Reproduce

Use the Windows Graphics Tools component, an Agility SDK 1.620.x x64
runtime directory, and DXC with Shader Model 6.6 support.

```powershell
$env:VICTORY_HIZ_TEST_AGILITY = 'C:\path\to\Agility\build\native\bin\x64'
$env:VICTORY_HIZ_TEST_DXC = 'C:\path\to\dxc.exe'
foreach ($adapter in 'amd', 'nvidia', 'warp') {
    $env:VICTORY_HIZ_TEST_ADAPTER = $adapter
    cargo test --locked -p victory-gpu --features diagnostics --lib draw::hiz_tests::cpu_placed_depth_drives_hiz_culling_and_mesh -- --exact --ignored --test-threads=1 --nocapture
    if ($LASTEXITCODE) { throw "Hi-Z validation failed on $adapter" }
}
```

Run each adapter in its own process. The fixture asserts that the debug
layer and GPU-based validation are active, with resource-state tracking.
It compares all 9,216 source words and 2,304 reduction words with the CPU
reference. Both rounds range from 0.25 to 0.75 and have no zero depth
samples, so checking only those aggregates would miss a stale texture.
The sampled-view round writes `[1, 1, 1]` and colors 72 of 256 pixels.
The alias-write round writes `[0, 1, 1]` and colors zero pixels.
AMD Radeon Graphics, NVIDIA RTX 5070 Laptop GPU, and Microsoft WARP all
supported mesh shaders and passed both rounds. No measured game timings
come from this fixture.

## Interpretation

The fixture uses the runtime's placed SRV materialization, reflection
table preparation, layout, upload, state tracking and queue submission.
It uses authored DXIL for compute, culling, mesh and pixel execution,
with a native indirect signature. The title's shader translation, exact
indirect ABI, alternate descriptor paths and original workload are
outside the fixture. Hardware without a mesh tier still checks depth,
the reduction and the native argument buffer.

At `ffe310e`, the texture refresh passes `guarded=false` to the memory
journal and hashes committed physical views when write watching is
unavailable. This fixture verifies that the hash path sees a write through
another view. #1122 fixes a distinct stale-data path for guarded
buffer views. #1273 covers output textures bound via placed UAVs. The
fixture binds its reduction output through an ordinary UAV.
#1135 narrows the CPU detile on changed subresources while preserving
the full native upload. [#1122](https://github.com/Marvisak/VIctory/pull/1122)
· [#1273](https://github.com/Marvisak/VIctory/pull/1273)
· [#1135](https://github.com/Marvisak/VIctory/pull/1135)

For the RTX 5080 report, the next discriminator is whether the exact
placed binding has a valid host descriptor and whether the title bytes,
native source, Hi-Z mip, or indirect groups first become flat. Compare
the same scene and cache state. Report build and driver, per-resource
format and extent, upload admission and refresh mode, aggregate min/max
and nonzero counts, an aggregate checksum or stable probe for each stage,
and indirect zero counts. Resource IDs are process local. Keep raw
captures, title data, and shader binaries local. [Issue #71 report](https://github.com/Marvisak/VIctory/issues/71#issuecomment-7137520686)
