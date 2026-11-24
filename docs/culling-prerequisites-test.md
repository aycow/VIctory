# Instance-culling tester comparison

Commit `06ac830a74df0fff49073c28680118fb439b90c9` combines the Wave64
waterfall head `e49783014536623f3cdb64bf4066cd6ad384798a` with the bounded
structured-buffer alias head `79e88c646ea858ff9b1e9762833990d6bed31c16`.
It preserves the host wave sizes in the shader cache key and the VSH3
alias metadata in the cache payload. The branch includes no changes from
#185, #206 or #562.

The target hashes and earlier refusal reasons come from the two instance-culling
reports. [Unknown strides](https://github.com/Marvisak/VIctory/issues/71#issuecomment-7123577206) · [Wave64 behavior](https://github.com/Marvisak/VIctory/issues/71#issuecomment-7123625627)

- `fdd56d97a806bcca`
- `17af15c4015cb4b6`
- `55679a0547649d0b`
- `89d08e561897d4e5`

## Matched runs

Build the same commit once and use the same executable, title build, save,
GPU, driver, console mode, scene and frame interval for both runs. Match
cache warmup and all diagnostic settings. Restart the process between runs.

```powershell
cargo build --locked --profile diag -p victory --features diagnostics
Remove-Item -LiteralPath Env:VICTORY_RUNTIME_STRIDE_ALIASES -ErrorAction SilentlyContinue
# Run the title using the usual command for the baseline.
$env:VICTORY_RUNTIME_STRIDE_ALIASES = '1'
# Restart the same executable with the same title arguments for the candidate.
```

The Wave64 fix is active in both runs. The baseline refuses pipelines that
need runtime stride metadata. The candidate can bind exact, aligned
structured compute views within the scope described in
[runtime stride aliases](runtime-stride-aliases.md). Translation can still
refuse a program, or runtime binding can refuse an unsupported view. If
checking offline translation, use the same host wave range in each
`victory translate --list --validate --host-lanes MIN-MAX` run. Offline
translation has no title descriptor bindings.

## Report aggregate results only

Report the executable commit, adapter, driver, wave-size range, and whether
the matched interval was reached. For each listed hash, include the observed
translation or validation outcome, pipeline and descriptor binding refusal reason
where available, and confirmed dispatch count or producer-output count when
the diagnostics expose it. Use “unobserved” for results the local tools
cannot establish.

At the matched interval, note whether downstream mesh consumers executed
and whether depth, G-buffer, or lighting output gained coverage. Report
counts or a short description of the observed outcome. Keep title assets,
screenshots, shader binaries, raw target dumps, and full logs local.

## Synthetic verification

The combined commit passed all three shader-cache tests. The production
alias fixture passed on AMD Radeon, NVIDIA RTX 5070 Laptop GPU, and WARP
with full D3D12 GPU-based validation. The disabled case refused the alias
pipeline. The partial-mask waterfall fixture returned 128 reference words
in each variant on AMD and NVIDIA with full GPU-based validation. WARP
correctly refused the Wave64 fixture on this host because it exposed
neither Wave32 nor Wave64. [Alias fixture commands](runtime-stride-aliases.md#reproduce-on-windows) · [Wave64 fixture commands](wave64-partial-mask-waterfall.md#reproduce)
