# Computed address map with shared backing

This test-only routing on top of exact #1198 (`fe078c6`) maps explicit guest intervals to owned D3D12 resources. It uses an original GFX10 compute program translated and validated through the normal DXIL pipeline. The program performs `global_load_dwordx4`, plain `s_load_dword`, vector and scalar stores and `global_atomic_add`. It compares an ordinary shadow with shared mapped backing, replaces the backing for the same guest address between recordings, drops its external native references before execution, and verifies the unmapped-address result and output guards. The CPU address of the shared allocation differs from the guest address.

Build and run on Windows with Agility 1.620.2 and a Windows SDK DXIL validator installed. Repeat the command in a fresh process for each adapter.

```powershell
$env:VICTORY_TABLE_TEST_AGILITY = 'C:\path\to\Agility\build\native\bin\x64'
$env:VICTORY_TABLE_TEST_ADAPTER = 'amd' # or nvidia, warp
cargo test -p victory-gpu --features diagnostics --lib computed_map_supports_shared_and_shadow_backing -- --ignored --nocapture
```

## Cross-piece access after #1198 `ad558cd`

The expanded fixture includes overlapping source and destination mappings, a missing middle dword, an unaligned atomic, and an atomic for which only three bytes are mapped. Each complete dword uses the highest-priority mapping covering it. Missing dwords read zero and receive no store. An atomic requires four mapped bytes in one piece and a naturally aligned guest address. The synthetic overlap reads `[31, 47, 53, 59]` and checks that the destination split writes each value to its own native backing.

Exact `ad558cd` used the first native view for the whole vector and failed the overlapping read/store fixture on NVIDIA and WARP. The straight-line per-dword candidate passed the expanded fixture on AMD Radeon Graphics and WARP with full GPU-based validation. On RTX 5070 Laptop GPU, the DXIL validator accepted the candidate but native compute pipeline creation failed. An earlier vector fast-path plus split fallback gave HRESULT `0x8007000E` on that adapter and also failed the WARP hole case. The straight-line candidate was refused on NVIDIA both with and without GPU-based validation. It remains a candidate pending a smaller driver-compatible lowering.

## Earlier synthetic replay at `fe078c6`

The measurements below are from the earlier `a920242` fixture. In a fresh process set `VICTORY_MAP_TEST_BENCH=1`, `VICTORY_GPU_TIMING=1`, and `VICTORY_D3D12_DRED=1` before running that revision. Leave `VICTORY_MAP_TEST_OVERLAP` unset. This mode disables the debug layer and GPU-based validation. It uses a debug build, one recorded one-lane dispatch per submission, 10 warmups and 21 alternating-order samples per backing. The recorded list labels bracket the GPU dispatch. The CPU preparation counter is rounded down to microseconds, and submission wall time includes the fence wait. The initial PSO and recording costs are outside the measurement.

| Adapter | CPU prep, shadow → shared | GPU span, shadow → shared | Synchronous submit, shadow → shared |
|---|---:|---:|---:|
| AMD Radeon Graphics | 1 → 0 µs | 3.84 → 4.12 µs | 93.80 → 92.70 µs |
| RTX 5070 Laptop GPU | 2 → 0 µs | 18.59 → 18.53 µs | 122.60 → 115.90 µs |
| WARP | 2 → 0 µs | 3.00 → 3.00 µs | 37.80 → 35.30 µs |

These medians are for one 16-byte input and one 32-byte output. Timestamp spans include address-map and shader work. This fixture does not measure isolated DRAM access latency, bandwidth, working-set residency, runtime fallback, or title frame time. Runtime selection still uses only #1198's shadow route. The shared bindings are supplied directly by the fixture.
