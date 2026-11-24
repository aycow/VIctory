# Runtime structured-buffer aliases

The tested implementation is commit `6fe5ba53ed223c35e3a96bc088fbb9b363ac8502`
on `fix/runtime-stride-descriptor-aliases`. It follows the merge of
[#193](https://github.com/Marvisak/VIctory/pull/193) at `4322b61` and main
at `d2a4bae`. The original failure and independent repro are recorded on
[#193](https://github.com/Marvisak/VIctory/pull/193#issuecomment-7122402744).

## Binding and bounds

`VICTORY_RUNTIME_STRIDE_ALIASES=1` enables the new pipeline path.
With the option unset, pipelines requiring runtime stride aliases are refused
before a host pipeline is created. Ordinary pipelines keep their existing
bindings.

The emitter accepts exact compute descriptor-table entries for indexed,
nonformatted dword loads, stores and atomics at a dword-aligned immediate
byte offset. The host checks the descriptor kind, original buffer size,
record count, dword stride and 16-byte host-copy alignment before allocating
an alias. It rejects typed views, guest raw views, counter UAVs, computed
descriptor and byte offsets, and source windows that cannot be represented
as bounded raw views. A dispatch cannot use the same host copy through both
an SRV alias and a UAV alias.

The raw alias covers exactly the original structured view's byte window on
the same host copy. The original descriptor slot stays intact. Each
dispatch's metadata supplies the alias index, stride and record count.
The shader checks the element index and access width before touching the
alias. Out-of-bounds loads return zero, and stores and atomics are skipped.
When the shader reads the original descriptor's record-count dword, the
emitter obtains it from the same metadata.

The list owns the descriptor slots, backing copies and metadata until reset.
The slot lease retains the allocator that issued it even when the title
releases the heap object. There is no mutable heap-wide stride table.
[#206](https://github.com/Marvisak/VIctory/pull/206) addresses a different
case in which a structured view starts between elements in its host copy.
That case stays refused. Ordinary views retain their existing creation path.

## Validation

The focused fixture creates a real translated compute pipeline, binds via
`State::bind`, and checks the entire title buffer after host copyback.
It replaces the source and destination descriptors between recording two
lists, drops the title heap, then submits both lists. The matrix covers
dword widths 1 through 4, strides 8, 12, 16, 20 and 32, in-record and
out-of-record load and atomic offsets, out-of-range store indices, and
typed SRV refusal.

On Windows 11 build 26200, AMD Radeon Graphics (32.0.22042.1), NVIDIA
GeForce RTX 5070 Laptop GPU (32.0.15.8191), and Microsoft Basic Render
Driver WARP each passed all 12 dispatches under DXIL validation, the D3D12
debug layer and full GPU-based validation with Agility SDK 620. A separate
process with the option unset verified that the same translated shader
was refused before creating a host pipeline. Shader cache tests and strict
library Clippy passed.

The fixture uses distinct source and destination backing buffers and the
existing synchronous host submission and copyback path. Composition with
the asynchronous submission proposals, buffer state changes between
unrelated pipelines, Wine and in-game rendering remains unverified.

## Reproduce on Windows

Use the x64 DLLs from
[Microsoft.Direct3D.D3D12 1.620.2](https://www.nuget.org/packages/Microsoft.Direct3D.D3D12/1.620.2)
and a Windows SDK with `dxil.dll`. In PowerShell at the repository root,
set `$strideSdkBin` to the SDK package's `build/native/bin/x64` directory.

```powershell
$strideTarget = Join-Path $env:TEMP 'victory-runtime-stride-target'
$strideDlls = Join-Path $strideTarget 'debug/deps/D3D12'
New-Item -ItemType Directory -Force $strideDlls | Out-Null
Copy-Item -LiteralPath @(
    (Join-Path $strideSdkBin 'D3D12Core.dll')
    (Join-Path $strideSdkBin 'd3d12SDKLayers.dll')
) -Destination $strideDlls
$env:CARGO_TARGET_DIR = $strideTarget
$env:VICTORY_RUNTIME_STRIDE_ALIASES = '1'
foreach ($adapter in 'amd', 'nvidia', 'warp') {
    $env:VICTORY_TEST_STRIDE_ADAPTER = $adapter
    cargo test --locked -p victory-gpu --features diagnostics --lib runtime_stride_aliases_production_bind -- --ignored --nocapture --test-threads=1
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
Remove-Item -LiteralPath Env:VICTORY_RUNTIME_STRIDE_ALIASES
cargo test --locked -p victory-gpu --features diagnostics --lib runtime_stride_aliases_production_bind -- --ignored --nocapture --test-threads=1
```
