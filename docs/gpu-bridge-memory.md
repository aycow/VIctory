# GPU bridge memory attribution

Set `VICTORY_MEMORY_STATS=1` in a Windows diagnostics build. After device creation, `victory_gpu::memory` logs a sample every three seconds, including during loading before the first present. Use elapsed time and the frame token to match loading phases. Leave the variable unset for ordinary runs.

The sampler combines process private commit, working set, cumulative page faults, system commit and its limit, node-zero DXGI local and non-local usage, and capacities of tracked owners. The DXGI fields are optional because the selected adapter might not expose them. These measurements overlap and cannot be added together.

- `shadow_bytes` counts the rounded buffer widths held by live shadow owners. `cached_shadow_guest_bytes` sums the logical guest extents of cached shadows. `cached_shadow_guest_unique_bytes` takes their union in guest virtual address space. The difference measures overlap at the same guest addresses. The sample cannot identify aliases mapped at different virtual addresses and excludes uncached shadows still held by recordings.
- `snapshot_bytes` counts CPU snapshot capacity in active snapshots, including both allocations while one grows. `idle_snapshot_bytes` covers the separate retained snapshot pool from #602.
- `list_scratch_bytes` counts chunks kept for command-list reuse, including after reset. `list_upload_bytes` counts dedicated uploads retained by the recording.
- `texture_upload_bytes` covers live staging owners. `cached_texture_upload_bytes` is a subset, and the separate texture read capacity belongs to the existing #602 counter.

Each tracked owner's high-water counter measures peak capacity during the process. The categories can peak at different times. Sampling uses atomic reads and nonblocking registry and retained-pool queries. A skipped sample is missing data. Requested buffer capacities are distinct from private commit and driver residency. The instrumentation covers the observable coexistence of snapshot buffers during growth. It does not track allocator-internal retention or resolve private commit by allocator, descriptor heap reservation, compiler allocation, pipeline-library blobs, readback pools, DirectStorage intermediates or async retirement.

## Synthetic validation

```powershell
$env:VICTORY_MEMORY_STATS = '1'
cargo test -p victory-gpu --features diagnostics --lib observes_shadow_and_list_capacity_through_reset_and_release -- --ignored --nocapture
```

Run the command in a new process for each adapter. `VICTORY_D3D12_WARP=1` selects WARP in this source tree. The fixture creates a 4 MiB live shadow. Its snapshot grows from 2 to 4 MiB while both allocations briefly coexist, so the diagnostics observe a 6 MiB active peak. Copyback moves the 6 MiB to the idle pool. Reset releases a 5 MiB dedicated upload and retains the 4 MiB scratch chunk until list release. Default AMD and WARP both passed the capacity checks and returned process and system commit samples. The fixture exercises allocation lifetime, with no GPU dispatch.

## Title measurements

For a matched #471 load, compare the same configuration and approximate frame window with the same adapter, driver, cache state and host settings. Collect the aggregate memory fields and the phase or failure time. The before-first-present sampler is useful when loading fails before the existing frame statistics can print. Do not share title memory, assets, addresses or shader identifiers.

This branch builds on #484 and #602 with their original authorship. #472 changes the underlying allocation policy and is a separate comparison. The synthetic fixture establishes attribution and lifetime tracking. World-load savings and sustained memory growth require matched title samples.
