# Numerical Wave32 masks in mesh shaders

On main `ffe310e`, an original mesh shader that takes the population count
of a comparison mask is refused when its primitive export needs the
numeric result. The refusal occurs during translation, before a D3D12
device or pipeline is selected. The companion shader with the same vertex
and primitive exports translates without the numerical mask.

The mesh stage cannot declare a fixed Wave32. Shader Model 6.6 exposes the
attribute on compute entry points, and the DXIL validator restricts wave
size metadata to compute and node shaders. An adapter reporting 32 to 64
lanes can choose its native mesh wave size. [WaveSize specification](https://microsoft.github.io/DirectX-Specs/d3d/HLSL_SM_6_6_WaveSize.html) · [DXIL validator rules](https://github.com/microsoft/DirectXShaderCompiler/blob/main/docs/DXIL.rst)

The translator lowers an eligible mesh ballot using flattened group thread
IDs. A distinct groupshared array isolates the scratch from console LDS.
Each logical 32-lane wave accumulates bits in its own cell. Group barriers
order initialization, contributions, and reads before scratch reuse.
The helper reserves four bytes per thread and checks the combined
28 KiB mesh groupshared limit. The first numeric use and the mask
definition must be in the same group-uniform native entry block.
The tracked mask must describe lanes present at its definition.
[D3D12 mesh shader limits](https://microsoft.github.io/DirectX-Specs/d3d/MeshShader.html)

## Reproduction

The CPU tests cover 32, 64, and 128 lanes per group, a separate
native LDS allocation, and refusal controls for a divergent ballot, Wave64,
and excess scratch. Set the SDK validator path explicitly for validation.

```powershell
$env:VICTORY_DXIL = 'C:\path\to\dxil.dll'
cargo test --locked -p victory-shader --lib mesh_numeric_wave32_masks -- --nocapture
```

The native test needs Windows Graphics Tools and an Agility SDK 1.620.x
x64 runtime directory. It selects a named adapter, verifies the mesh
tier and full GPU-based validation, and checks error and corruption
messages. Each of two logical Wave32s emits its own triangle only if both
the sparse ballot bits and the expected counts match. A complementary
ballot exercises scratch reuse. The test reads back one interior pixel
from each triangle and runs a companion shader without numeric masks.

```powershell
$env:VICTORY_MESH_MASK_TEST_SDK = 'C:\path\to\Agility\build\native\bin\x64'
$env:VICTORY_MESH_MASK_TEST_ADAPTER = 'amd' # or nvidia, warp
cargo test --locked -p victory-shader --lib mesh_numeric_wave32_masks_control_two_triangles -- --ignored --nocapture --test-threads=1
```

The measured AMD Radeon Graphics adapter reported mesh tier 1 and
32–64 lanes. Agility WARP reported tier 1 and four lanes. Both rasterized
the expected pixels with GPU-based validation active. NVIDIA was absent
from DXGI enumeration during this test window.

## Coverage

The lowering leaves divergent numerical ballot creation, mask joins
without the existing tracked-mask proof, and general mesh wave operations
outside its scope. #439's compute console-flow work, #1407's masks that
hold lanes outside the writing block, #1269's wave-uniform SCC handling,
and #1563's mesh output linkage cover separate cases.

The result establishes synthetic mesh translation and readback. The
barrier and atomic costs have no benchmark measurements. A matched title
run can report aggregate admitted and refused mesh counts, submitted and
output primitive counts, depth or G-buffer coverage, and whether the final
image changes. Record the adapter, driver, executable commit, scene,
cache warmup and diagnostics. Keep title shaders, captures and assets local.
