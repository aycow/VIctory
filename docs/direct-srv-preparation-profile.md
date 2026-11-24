# Direct SRV preparation profiling

This branch combines fork PR #2 with the current #185 reservation
classification fix. It adds diagnostics for CPU preparation stages.
The underlying batching policy comes from the contributor's PR.

## Matched title measurements

Build with `cargo build --locked --profile diag -p victory --features diagnostics`.
Run a warm-up for each exact revision and settings combination. Set
`VICTORY_PROFILE_SHARED_PREP=1` and collect the same steady scene window
with shared spans enabled and with `VICTORY_NO_SHARED_SPANS=1`. Start a
fresh process when changing these flags. Hold the adapter, driver,
resolution, profile, scene and other diagnostics options fixed.

Share the aggregate `direct SRV preparation split per frame`,
`direct SRVs per frame` and `shadow traffic per frame` lines, plus
frame and submission counts and the matching submit preparation and
queue-completion intervals. Use the same summary interval across fields.
Keep title assets, raw addresses and full logs local. Repeat with
profiling disabled for timing impact.

The split measures ready-range collection, flattening and sorting,
reservation grouping and fallback bookkeeping, shared observation
including the journal and oldest generation lookup, applying the
observations to host copies, and the per-shadow fallback. Applying an
observation can include CPU copies. With sharing disabled the old
per-shadow path is reported under fallback. The extra timers add CPU
work, especially for many small groups.

## Synthetic control

Use the Windows Graphics Tools optional feature, DXC from the Windows
SDK, and Agility SDK 1.620.2 x64 DLLs under
`target/release/D3D12`. Set `VICTORY_TEST_AGILITY=1`.

```powershell
$env:VICTORY_PC_DXC = 'C:\path\to\dxc.exe'
$env:VICTORY_COH_PROFILE_SHARED_SPANS = '1'
$env:VICTORY_COH_PROFILE_SHARED_LAYOUT = 'dense' # or thin or gapped
$env:VICTORY_PROFILE_SHARED_PREP = '1'
cargo test --release --locked -p victory-gpu --features diagnostics directly_indexed_srvs_of_neighbouring_copies_share_a_question -- --nocapture
```

Use an AMD adapter by default, `VICTORY_TEST_SRV_NVIDIA=1` for NVIDIA,
or `VICTORY_D3D12_WARP=1` for WARP. The fixture reads one dword from
an original synthetic buffer and checks the returned value after every
submission. It records two command lists once and warms both. Each of
three recorded batches resubmits the lists 80 times in total.

All 64 views are tracked for preparation. The dense layout spaces the
views by 64 KiB, the thin layout spaces them by 1 MiB and still permits
one shared question, and the gapped layout spaces them by 2 MiB and
uses per-shadow questions. Toggle only `VICTORY_NO_SHARED_SPANS` for
the matched control. Omit `VICTORY_PROFILE_SHARED_PREP` for timing the
same batches without the split timers. The input data, output checks,
and native commands match across the two paths.

## Windows synthetic results

The table gives the median of nine timed batches for each configuration
on AMD Radeon Graphics in a release build, with validation and the added
stage timers disabled. Each configuration was run in three fresh
processes, alternating which mode ran first in each pair.

| Layout | Host preparation per 80 submissions, off → on | Successful watch calls, off → on |
| --- | ---: | ---: |
| Dense, 64 KiB spacing | 3.061 → 1.344 ms | 5,120 → 80 |
| Thin, 1 MiB spacing | 3.205 → 4.119 ms | 5,120 → 80 |
| Gapped, 2 MiB spacing | 3.313 → 3.581 ms | 5,120 → 5,120 |

In separate profiled batches with sharing on, the shared observation
took a median 0.481 ms in the dense layout and 3.904 ms in the thin
layout. The latter included 3.702 ms in successful native write-watch
calls. In the gapped layout the measured sort and grouping stages took
0.131 and 0.359 ms before the existing fallback. The timer measures
coarse stages and includes its own observation cost. It reports queue
submission through fence completion separately. These are synthetic
preparation measurements, separate from the in-game Proton report.
