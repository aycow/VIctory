# Shared arena for runtime-owned UAV budget words

This follow-up to #407 and #501 adds a runtime allocation path for
indirect-work budget words. Build with diagnostics and set
`VICTORY_SHARED_OWNED_UAV=1` to enable it. The default still allocates
a committed buffer per budget.

The arena is one resident-by-default 4 MiB WRITE_BACK/L0 heap with 64
fixed 64 KiB slots. Each 256-byte budget is a separate placed resource.
Its allocation and alignment are checked against the device. The list
owns the resource and lease, records an alias activation before using
it, and zeros the mapped word during submission preparation. Reuse
after reset keeps the lease. Destroying the completed list releases the
resource before returning its slot. The submission must establish the
fence completion before returning a placed budget to CPU ownership.
An exhausted or unsupported arena uses the existing committed path
for that budget.

Fixed-size slots can be reclaimed independently. The heap stays allocated
for the host's lifetime under system residency management. Eviction and
allocation pressure in a title need separate measurements. This uses
the synchronous host queue contract. An asynchronous port must move
the lease and mapped word into the actual completion timeline.

## Reproduce

```powershell
$env:VICTORY_SHARED_TEST_SDK = 'C:\path\to\agility\build\native\bin\x64'
$env:VICTORY_SHARED_TEST_ADAPTER = 'amd' # repeat with nvidia and warp
$env:VICTORY_SHARED_OWNED_UAV = '1' # repeat with 0 for committed control
cargo test --release --locked -p victory-gpu --features diagnostics owned_uav_budget_uses_completed_leases -- --ignored --nocapture --test-threads=1
```

The fixture records through the actual host-list budget allocator.
It executes an original 1,024-thread atomic increment shader and
checks the mapped word after the fence. It also verifies independent
live allocations, post-fence reset and slot reuse, a nonidentity CPU/GPU
address, and a committed allocation with correct readback after pool
exhaustion. AMD Radeon Graphics, NVIDIA GeForce RTX 5070 Laptop, and
WARP passed both paths with Agility SDK 1.620.2, the D3D12 debug layer,
and full GPU-based validation.

## Synthetic timings

Set `VICTORY_SHARED_TEST_TIMING=1` to disable validation for timing.
Test inputs and GPU output checks remain the same. Each path uses three
warm-up dispatches and ten timestamped 1,024-thread dispatches. The
table uses the median of five fresh processes per path, alternating
execution order. Allocation time is CPU wall time for creating and
mapping 64 budget resources with the arena already initialized.
Dispatch time is the native queue timestamp interval, excluding
activation, preparation, and fence waiting.

| Adapter | CPU allocation for 64 budgets, committed → placed | GPU atomic dispatch, committed → placed |
| --- | ---: | ---: |
| AMD Radeon Graphics | 30.898 → 0.077 ms | 0.480 → 0.800 µs |
| NVIDIA RTX 5070 Laptop | 13.412 → 0.209 ms | 24.416 → 1.408 µs |
| WARP | 3.056 → 0.181 ms | 18.800 → 17.700 µs |

Measured submission preparation was at the timer's 0–1 µs resolution.
These results apply to one contended atomic word and the tested
allocation pattern. Discrete-GPU bandwidth and in-game performance
require separate measurements.

Guest memory retains the shadow path. Guest routing would require
stable backing across remaps, an explicit handoff protocol for CPU and
GPU writes, and an independently verified fallback on each allocation.
The #407 imported-section discrepancy on Proton remains relevant to that
route. Wine, guest aliasing, and eviction and residency pressure remain
unvalidated here.
