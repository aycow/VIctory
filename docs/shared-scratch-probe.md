# Opt-in shared scratch allocator

Build with `cargo build --locked --profile diag -p victory --features diagnostics`.
Set `VICTORY_SHARED_SCRATCH=1` to use the mapped arena from #501 for
standard command-list scratch chunks. The option defaults off. These
allocations are owned by the runtime and appear in GPU read-only bindings.
They include scratch constants and generated vertex data. The native queue
activates the placed resource once before use. Each list retains its chunk
and only reuses it on reset under the synchronous submission contract.
The CPU and GPU addresses have independent bases and the same byte offset.

The arena uses a 256 MiB write-back/L0 heap and 256-byte alignment. An
unsupported or exhausted arena uses an ordinary committed upload buffer
for the allocation. Large scratch requests continue using that path.
The arena reserves space monotonically, so churn can reach the fallback
even when older lists are gone.

## Synthetic validation and timings

Use Windows Graphics Tools and the Agility SDK 1.620.2 x64 DLL directory:

```powershell
$env:VICTORY_SHARED_SCRATCH = '1'
$env:VICTORY_SHARED_TEST_SDK = 'C:\path\to\agility\build\native\bin\x64'
foreach ($adapter in 'amd', 'nvidia', 'warp') {
    $env:VICTORY_SHARED_TEST_ADAPTER = $adapter
    cargo test --release --locked -p victory-gpu --features diagnostics host::scratch_tests::shared_scratch_matches_shadow -- --exact --ignored --nocapture
    if ($LASTEXITCODE -ne 0) { throw "Validation failed on $adapter" }
}
```

AMD Radeon Graphics, NVIDIA GeForce RTX 5070 Laptop and WARP passed with
the debug layer and full GPU-based validation. The fixture verifies that
the mapped pointer and native GPU address agree at a nonzero offset,
the selected scratch chunk survives a list reset, and an allocation
after exhaustion returns correct data through the upload fallback.

Set `VICTORY_SHARED_TEST_TIMING=1` to repeat the release-mode benchmark
with validation disabled. Each dispatch reads one MiB through either an
ordinary title-memory shadow or the arena chunk. The first pass warms each
path. The fixture alternates order over five measured pairs for each of
the three dword strides. It compares all GPU output words with the CPU
reference. It times data generation, host submission preparation, and
submission through queue-fence completion separately.

| Adapter | Shadow preparation, strides 1/17/257 | Arena preparation, strides 1/17/257 |
| --- | ---: | ---: |
| AMD Radeon Graphics | 73 / 49 / 54 µs | 1 / 0 / 1 µs |
| NVIDIA RTX 5070 Laptop | 58 / 80 / 89 µs | 0 / 0 / 1 µs |
| WARP | 68 / 70 / 76 µs | 1 / 1 / 1 µs |

The baseline uses one ordinary private allocation filled each iteration.
The timer excludes list creation, the arena's initial activation, pipeline
creation, and readback verification. The queue-fence interval includes
submission and wait overhead and can vary. The test estimates the cost of
refreshing this particular shadow, rather than an in-game frame or GPU
bandwidth. The runtime scratch A/B is arena versus committed upload
allocation, while the fixture's shadow comparison models possible future
allocation routing.

## Scope of the integration

Title allocations retain their existing addresses and shadow-copy path.
Initial import experiments for address and section-backed memory on
Windows are recorded separately in #407. The section path has a reported
correctness discrepancy on Proton. Guest alias/remap semantics, residency
and memory pressure, asynchronous writes, and the arena path on Wine
remain unvalidated. The fixture exercises disjoint read-only scratch under
the existing queue fence, including fallback when the arena runs out.
