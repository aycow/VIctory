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

The CPU tests cover 17, 32, 47, 64, and 128 threads per group, a separate
native LDS allocation, and refusal controls for a divergent ballot, Wave64,
and excess scratch. Set the SDK validator path explicitly for validation.

```powershell
$env:VICTORY_DXIL = 'C:\path\to\dxil.dll'
cargo test --locked -p victory-shader --lib mesh_numeric_wave32_masks -- --nocapture
```

The native test needs Windows Graphics Tools and an Agility SDK 1.620.x
x64 runtime directory. It selects a named adapter, verifies the mesh
tier and full GPU-based validation, and checks error and corruption
messages. Each logical Wave32 emits its own triangle only when the sparse
ballot bits and the expected population count match. The complementary
and third ballots exercise scratch reuse. Nested EXEC branches consume the
earlier result. The test checks four independent render-target regions
against a cleared background, with a deliberate wrong-mask control.

```powershell
$env:VICTORY_MESH_MASK_TEST_SDK = 'C:\path\to\Agility\build\native\bin\x64'
$env:VICTORY_MESH_MASK_TEST_ADAPTER = 'amd' # or nvidia, warp
cargo test --locked -p victory-shader --lib mesh_numeric_wave32_masks_control_two_triangles -- --ignored --nocapture --test-threads=1
```

The 15 cases passed on AMD Radeon Graphics, driver 32.0.22042.1 with
32–64 lanes, and the Agility WARP renderer with four lanes. Both exposed
mesh tier 1 and SM 6.6. The 120 pixel probes include Wave32 groups of
17, 31, 32, 33, 47, 64, 96, and 128 threads, logical Wave64 controls at
64 and 128 threads, and a separate native LDS allocation. AMD's actual
physical mesh wave width was not measured. NVIDIA was absent from DXGI.

## Synthetic GPU cost

Use the same SDK and select a physical adapter. The benchmark creates
fresh named-adapter devices with the debug layer and GPU-based validation
disabled. It times otherwise matching raster workloads with 0–3 distinct
numerical ballots. Two direct-queue GPU timestamps enclose 8 dispatches of
2,048 groups each. Target setup, the resolve, readback, CPU preparation and
PSO creation fall outside the timed region. Each process rotates the
pipeline order and retains 15 samples per point after three warmups.

```powershell
$env:VICTORY_MESH_MASK_TEST_ADAPTER = 'amd'
cargo test --locked -p victory-shader --lib mesh_wave32_ballot_gpu_cost -- --ignored --nocapture --test-threads=1
```

Three alternating runs on the measured AMD adapter gave these medians
of the per-process medians in GPU microseconds per 2,048-group dispatch.

| Threads per group | No ballots | One ballot | Two ballots | Three ballots |
|---|---:|---:|---:|---:|
| 32 | 56.36 | 58.83 | 80.17 | 101.58 |
| 64 | 112.03 | 115.09 | 130.67 | 172.17 |
| 128 | 198.61 | 199.26 | 251.59 | 341.98 |

The tested interval includes raster work plus mask arithmetic, shared atomics,
and barriers. Zero-ballot timing also drifted across runs. The one-ballot
differences were within that drift.

The separate scratch-bank experiment at `8378317` allocates two banks
when space allows and performs two group barriers per ballot. It falls back
to the three-barrier path near the 28 KiB memory limit. Both raster suites
and the three-ballot wrap passed with full GPU-based validation. With each
run normalized by its own zero-ballot control, the two-bank experiment
reduced the 128-thread three-ballot interval by 1.6–10.2% across three
paired rounds. The 32- and 64-thread three-ballot ratios varied in both
directions. Keep the original scratch path for this proposal. The timing
does not isolate one barrier instruction's cost.

## Coverage

The lowering leaves divergent numerical ballot creation, mask joins
without the existing tracked-mask proof, and general mesh wave operations
outside its scope. #439's compute console-flow work, #1407's masks that
hold lanes outside the writing block, #1269's wave-uniform SCC handling,
and #1563's mesh output linkage cover separate cases.

The result establishes synthetic mesh translation and readback. The
uniform-entry constraint also keeps synchronization out of lowered
divergent DXIL blocks. Extending support would require a proof that the
same threads reach each exchange in the same order and that the mask's
operands dominate that point. A mask materialized before a divergent
consumer already works in the tested cases.

A matched title run can report aggregate admitted and refused mesh counts, submitted and
output primitive counts, depth or G-buffer coverage, and whether the final
image changes. Record the adapter, driver, executable commit, scene,
cache warmup and diagnostics. Keep title shaders, captures and assets local.
