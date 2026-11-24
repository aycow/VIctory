# GPU execution performance census

Build with diagnostics and set VICTORY_GPU_CENSUS=1 before running the title.

~~~powershell
cargo build --profile diag -p victory --features diagnostics
$env:VICTORY_GPU_CENSUS = '1'
$env:VICTORY_LOG = 'info'
~~~

The victory_gpu::census log target emits approximately five-second windows on completed submissions and flushes pending data when the title thread returns or the host reports a GPU failure.

Execution rows are sorted by accumulated milliseconds and grouped by 32-digit pipeline key and command kind. They show stage and 16-digit shader hashes, recorded calls, calls per second, timed calls, median, maximum, and accumulated time. Shader rows aggregate spans across pipelines using that shader. Mesh and pixel shaders in the same pipeline share the same span. Refusal rows count the runtime’s existing classifications across the census window. CPU build rows use the same pipeline key and include translation on a miss, root signature work, and PSO load or creation.

Direct compute and mesh rows report group totals and maximum dimensions. Draw rows report vertex or index counts times instances. Clamp rows count candidate argument records. Indirect rows report ExecuteIndirect calls, stride, and exact argument record counts when there is no GPU count buffer. For counted calls they report how many calls had a count buffer and the sum of their upper limits. Obtaining the effective GPU count, nonempty-record count, and group totals would require separate capture. The census adds no resource transition or data readback for those fields.

Each timestamp pair marks two completion points on the direct queue. Other work can overlap, and timestamp queries can affect scheduling. Use the spans to rank where to investigate. Summing them cannot yield exclusive GPU time or frame time. The census resolves into list-owned buffers and consumes them after the existing fence, with no added submission, fence wait, or resource barrier. The first 2,048 operations of a list receive timestamp slots. Later calls, duplicate instances of a list in one batch, and unavailable results still contribute invocation counts. The calls-versus-timed difference shows coverage. Time both runs with the same capture settings, and run debug-layer and GPU-based validation separately. [D3D12 timestamps](https://learn.microsoft.com/en-us/windows/win32/direct3d12/timing) · [Command list batching](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12commandqueue-executecommandlists) · [Indirect count semantics](https://learn.microsoft.com/en-us/windows/win32/direct3d12/nf-d3d12-id3d12graphicscommandlist-executeindirect)

PR #130 profiles frame pacing, CPU fence waits, load scopes, caches, and memory. GPU timestamp collection is a follow-up in its description. PR #279 improves DRED stop attribution and adds shader dumps keyed by the existing shader hash. The census uses structured metadata and keeps DRED independent. When both timing switches are set, the census takes precedence to retain normal batching. [#130](https://github.com/Marvisak/VIctory/pull/130) · [#279](https://github.com/Marvisak/VIctory/pull/279)

### Census validation

The original compute and mesh fixture records two lists and submits them in one batch. It validates direct and indirect outputs, group and record metadata, timestamp coverage, and exactly one submission even with the legacy timing switch also set. The single-record smoke passed on AMD Radeon Graphics (32.0.22042.1), RTX 5070 Laptop (32.0.15.8191), and WARP with the debug layer and full GBV. Agility 1.620.2 emits a feature-query supersession advisory. The test checks all other warnings and errors.

With a limit of three, NVIDIA and WARP wrote outputs for all three fixed records and the first two counted records, as expected. On the AMD adapter, the native fixture wrote only the first record in both calls. That result also occurred with timing disabled. A second unprofiled check supplied a zero count and still observed the first record on AMD. These findings concern this adapter and driver in the synthetic native test. They warrant independent confirmation before any change to VIctory’s indirect execution. [ExecuteIndirect contract](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12graphicscommandlist-executeindirect)

~~~powershell
$env:VICTORY_GPU_CENSUS = '1'
$env:VICTORY_GPU_TIMING = '1'
$env:VICTORY_CENSUS_TEST_AGILITY = '<Agility SDK 1.620.x x64 runtime directory>'
$env:VICTORY_CENSUS_TEST_DXC = '<path to x64 dxc.exe>'
foreach ($adapter in 'amd', 'nvidia', 'warp') {
    $env:VICTORY_CENSUS_TEST_ADAPTER = $adapter
    cargo test -p victory-gpu --features diagnostics census::gpu_tests::census_gpu_smoke -- --exact --ignored --nocapture
}
~~~

Set VICTORY_CENSUS_TEST_MULTI_INDIRECT=1 for the three-record probe or VICTORY_CENSUS_TEST_ZERO_INDIRECT=1 for a zero GPU count. Clear both timing switches for an unprofiled control. The regular census unit test checks exact median, maximum and sum, work counts, and the accounting for untimed and counted calls.

## Matched title measurements

Use a title copy you are authorized to run and share diagnostics from. Hold the adapter, driver, title build, save, inputs, PR stack, loop settings and cache warm-up fixed. Collect the same opening-cutscene frame range, and label partial windows or device removal. Each census window records its start and end present-call counts.

Measure an unmodified-main frame and submission baseline first. For a per-shader optimization comparison, build this PR on both sides and add one candidate change to the second build. Set VICTORY_GPU_CENSUS=1 on both. Unset VICTORY_GPU_TIMING, DRED, the debug layer and GPU-based validation for timing. Collect correctness and hang diagnostics in separate runs.

Share the build SHAs, GPU and driver, frame range and count, and aggregate rows for expensive compute and mesh pipelines. Include shader and pipeline hashes, calls and timed calls, direct groups or indirect limits, median, maximum and accumulated milliseconds, and refusal counts by reason. Aggregate figures and hashes are sufficient.
