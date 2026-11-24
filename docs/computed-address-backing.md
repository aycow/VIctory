# Computed address map with shared backing

This test-only routing on top of exact #1198 (`fe078c6`) maps explicit guest intervals to owned D3D12 resources. It uses an original GFX10 compute program translated and validated through the normal DXIL pipeline. The program performs `global_load_dwordx4`, plain `s_load_dword`, vector and scalar stores and `global_atomic_add`. It compares an ordinary shadow with shared mapped backing, replaces the backing for the same guest address between recordings, drops its external native references before execution, and verifies the unmapped-address result and output guards. The CPU address of the shared allocation differs from the guest address.

Build and run on Windows with Agility 1.620.2 and a Windows SDK DXIL validator installed. Repeat the command in a fresh process for each adapter.

```powershell
$env:VICTORY_TABLE_TEST_AGILITY = 'C:\path\to\Agility\build\native\bin\x64'
$env:VICTORY_TABLE_TEST_ADAPTER = 'amd' # or nvidia, warp
cargo test -p victory-gpu --features diagnostics --lib computed_map_supports_shared_and_shadow_backing -- --ignored --nocapture
```

The correctness cases passed on AMD Radeon Graphics, NVIDIA RTX 5070 Laptop GPU, and WARP with full GPU-based validation. To check a distinct bounds problem, set `VICTORY_MAP_TEST_OVERLAP=1`. The extra vector spans two guest mappings, while the outer native view can fit all four words. Proper map-based resolution would return `[31, 47, 53, 59]` from both mappings. The current lowering uses the first dword's mapping for the entire vector.

## Measured synthetic replay

In a fresh process set `VICTORY_MAP_TEST_BENCH=1`, `VICTORY_GPU_TIMING=1`, and `VICTORY_D3D12_DRED=1` before running the same command. Leave `VICTORY_MAP_TEST_OVERLAP` unset. This mode disables the debug layer and GPU-based validation. It uses a debug build, one recorded one-lane dispatch per submission, 10 warmups and 21 alternating-order samples per backing. The recorded list labels bracket the GPU dispatch. The CPU preparation counter is rounded down to microseconds, and submission wall time includes the fence wait. The initial PSO and recording costs are outside the measurement.

| Adapter | CPU prep, shadow → shared | GPU span, shadow → shared | Synchronous submit, shadow → shared |
|---|---:|---:|---:|
| AMD Radeon Graphics | 1 → 0 µs | 3.84 → 4.12 µs | 93.80 → 92.70 µs |
| RTX 5070 Laptop GPU | 2 → 0 µs | 18.59 → 18.53 µs | 122.60 → 115.90 µs |
| WARP | 2 → 0 µs | 3.00 → 3.00 µs | 37.80 → 35.30 µs |

These medians are for one 16-byte input and one 32-byte output. Timestamp spans include address-map and shader work. This fixture does not measure isolated DRAM access latency, bandwidth, working-set residency, runtime fallback, or title frame time. Runtime selection still uses only #1198's shadow route. The shared bindings are supplied directly by the fixture.
