# GPU bridge memory attribution

Set `VICTORY_MEMORY_STATS=1` in a Windows diagnostics build. After device creation, `victory_gpu::memory` logs a sample every three seconds, including during loading before the first present. Use elapsed time and the frame token to match loading phases. Leave the variable unset for ordinary runs.

The sampler combines process private commit, working set, cumulative page faults, system commit and its limit, node-zero DXGI local and non-local usage, and capacities of tracked owners. The DXGI fields are optional because the selected adapter might not expose them. These measurements overlap and cannot be added together.

- `shadow_bytes` sums actual D3D12 buffer widths held by live shadow owners, including the smaller alignment of a view-specific copy. `cached_shadow_guest_bytes` sums logical guest extents of registered buffers, loose windows and live view-specific cache entries. `cached_shadow_guest_unique_bytes` takes their union in guest virtual address space. The difference measures overlap at the same guest addresses. Distinct virtual aliases and uncached shadows still held by recordings fall outside the extent sample.
- `snapshot_bytes` counts CPU snapshot capacity in active snapshots, including both allocations while one grows. `idle_snapshot_bytes` covers the separate retained snapshot pool from #602.
- `list_scratch_bytes` counts chunks kept for command-list reuse, including after reset. `list_upload_bytes` counts dedicated uploads retained by the recording.
- `texture_upload_bytes` and `texture_read_bytes` cover live staging owners and their retained CPU read buffers. The #602 frame sample reports the same capacities, including staging still held by a recording. Use one sample stream for each comparison. A version with shared detile scratch would need to account for its global buffer separately.
- `native_texture_bytes` sums the native allocation sizes of live `HostTexture` owners. `native_used_bytes` counts the live portion marked used by a list at least once. These counters can miss native resources retained by a descriptor or another COM owner after `HostTexture` is dropped. DXGI segment usage also includes resources outside the tracked owners.

Each tracked owner's high-water counter measures peak capacity during the process. The categories can peak at different times. Sampling uses atomic reads and nonblocking registry and retained-pool queries. A skipped sample is missing data. Requested buffer capacities are distinct from private commit and driver residency. The instrumentation covers the observable coexistence of snapshot buffers during growth. It does not track allocator-internal retention or resolve private commit by allocator, descriptor heap reservation, compiler allocation, pipeline-library blobs, readback pools, DirectStorage intermediates or async retirement.

## Synthetic validation

```powershell
$env:VICTORY_MEMORY_STATS = '1'
cargo test --locked -p victory-gpu --features diagnostics --lib observes_shadow_and_list_capacity_through_reset_and_release -- --ignored --nocapture --test-threads=1
```

Run the command in a new process for each adapter. `VICTORY_D3D12_WARP=1` selects WARP in this source tree. The fixture creates a 4 MiB live shadow and a nested 456-byte view with a 512-byte buffer. The view's buffer width contributes 512 bytes to the live gauge, while the 456-byte logical range contributes to cached extent overlap only while the view has an owner. A snapshot grows from 2 to 4 MiB while both allocations briefly coexist, so the diagnostics observe a 6 MiB active peak. Copyback moves the 6 MiB to the idle pool. Reset releases a 5 MiB dedicated upload and retains the 64 KiB and 128 KiB on-demand scratch chunks until list release. The fixture exercises allocation lifetime and reports process and system commit.

With the fixture on `8b405d3`, Radeon Graphics reported a 512-byte resource width and charged 65,536 bytes. The corrected package charged 512 bytes and passed the capacity and lifetime checks on Radeon Graphics and WARP.

## Title measurements

For a matched #471 load, compare the same configuration and approximate frame window with the same adapter, driver, cache state and host settings. Collect the aggregate memory fields and the phase or failure time. The before-first-present sampler is useful when loading fails before the existing frame statistics can print. Do not share title memory, assets, addresses or shader identifiers.

This branch composes current main `ccfb029`, Andrea's #484 process counters, and lucaseddd's #602 staging allocation tracking at `380dab2`. It charges the actual on-demand scratch chunk capacity from #1246. On a busy owner, the optional #602 sample may be absent. The attribution control retains the per-texture CPU input allocation. #472 changes allocation policy and remains a separate comparison. [#484](https://github.com/Marvisak/VIctory/pull/484) · [#602](https://github.com/Marvisak/VIctory/pull/602) · [#472](https://github.com/Marvisak/VIctory/pull/472) · [#1246](https://github.com/Marvisak/VIctory/pull/1246)

Calvix2k's earlier comparison on `1eab79c` versus `dac1953` used matching #175 and #1798 overlays. On Series S, process private bytes were 10.39 GB and 10.19 GB at frames 1655 and 1660. CPU read capacity was 43,421,696 and 4,096 bytes. Upload capacity also changed from 43,628,192 to 4 bytes in the samples, even though scratch shares only the CPU detile input. The sample pair cannot isolate the cause of the upload or private-commit differences. Both Series X runs hit operating-system error 1455 while requesting another 64 KiB of physical-page commitment. An exact current-main matched capture is still required. [Revision A](https://github.com/Marvisak/VIctory/pull/602#issuecomment-7147967893) · [Revision B](https://github.com/Marvisak/VIctory/pull/602#issuecomment-7148019321)

The original RTX 5060 report measured 18.0 GB process private commit and 48.8 of 49.0 GB system commit at 141 seconds. Capi360's separate RTX 3060 Ti startup run used `8d67d8e` with a shared texture read buffer and reached 12.08 GiB of process private commit in its last system sample. The preceding retained-memory sample reported 38,153,480 bytes of staging, 4,096 bytes of shared read capacity, and zero idle snapshot bytes. That sample predates world entry and covers a different stack. Allocator attribution requires owner capacity and commit samples from the same load. [#471](https://github.com/Marvisak/VIctory/issues/471) · [Capi360's startup sample](https://github.com/Marvisak/VIctory/pull/602#issuecomment-7145001758)
