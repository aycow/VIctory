# Parallel hashing for untracked textures

`VICTORY_PARALLEL_TEXTURE_HASH=1` enables an optional two-worker BLAKE3 pool on hosts reporting at least four available logical processors. Only untracked inputs of at least 4 MiB use the pool. The existing serial digest remains the default and is used if the pool cannot be created. Both paths return the same BLAKE3 digest and use the existing change detection, detiling and upload paths. No copy of the input is allocated. [BLAKE3's parallel API](https://docs.rs/blake3/1.8.7/blake3/struct.Hasher.html#method.update_rayon)

## Synthetic reproduction

```powershell
cargo test -p victory-gpu --features diagnostics --release --locked texture::hash::tests::two_workers_preserve_the_serial_digest -- --exact
cargo test -p victory-gpu --features diagnostics --release --locked texture::hash::tests::compare_serial_and_parallel_hash_cost -- --exact --ignored --nocapture --test-threads=1
$env:VICTORY_PARALLEL_TEXTURE_HASH = '1'
cargo test -p victory-gpu --features diagnostics --release --locked texture::hash::tests::untracked_texture_skips_equal_bytes_and_uploads_a_changed_last_texel -- --exact --ignored
```

Run the last test in a fresh process with the switch unset for the serial control. Set `VICTORY_D3D12_WARP=1` to select WARP. The fixture uses a generated tiled RGBA8 texture and checks both the skipped refresh and the staging bytes after changing its last texel. It passed on AMD Radeon Graphics and WARP in both modes.

The CPU benchmark warms each path three times and alternates their order in 21 matched pairs for each generated input. It checks every digest against the serial control. It measures wall time for hashing alone. Pool startup, detiling, GPU execution and title activity are outside the measured spans.

| Tiled input | Serial median | Two-worker median |
| --- | ---: | ---: |
| 8,847,360 bytes | 2.124 ms | 1.219 ms |
| 33,423,360 bytes | 7.779 ms | 4.233 ms |
| 133,693,440 bytes | 34.866 ms | 20.200 ms |

## Complete refresh and worker costs

The release fixture runs the runtime's untracked refresh against a native mapped upload buffer. It includes the mapping check, BLAKE3 comparison and the tiled-to-linear write when a generated texture changes. It verifies the serial digest, the skip decision and the uploaded last texel. The test records caller and Rayon worker CPU time and cycles. It also records worker stack reservations and commitments and the process's private commit around the first digest.

```powershell
Remove-Item Env:VICTORY_PARALLEL_TEXTURE_HASH,Env:VICTORY_TEST_HASH_STACK_KIB -ErrorAction SilentlyContinue
cargo test -p victory-gpu --features diagnostics --release --locked texture::hash::benchmark::report_untracked_refresh_and_pool_cost -- --exact --ignored --nocapture --test-threads=1
$env:VICTORY_PARALLEL_TEXTURE_HASH = '1'
cargo test -p victory-gpu --features diagnostics --release --locked texture::hash::benchmark::report_untracked_refresh_and_pool_cost -- --exact --ignored --nocapture --test-threads=1
$env:VICTORY_TEST_HASH_STACK_KIB = '512'
cargo test -p victory-gpu --features diagnostics --release --locked texture::hash::benchmark::report_untracked_refresh_and_pool_cost -- --exact --ignored --nocapture --test-threads=1
```

On the Ryzen AI 7 350 Windows system with AMD Radeon Graphics, the six fresh processes ran in serial, parallel, 512 KiB stack, 512 KiB stack, parallel, serial order. Each case used three warmups and 21 timed rounds. Concurrent callers refreshed distinct textures and started each round together. The time for a round is the slowest refresh in that round. The ranges below contain the two process medians for each configuration. The fixture also prints each process maximum.

| Source | Callers | Unchanged serial → parallel | Changed serial → parallel |
| --- | ---: | ---: | ---: |
| 4 MiB | 1 | 0.590–0.665 → 0.394–0.419 ms | 1.160–1.192 → 0.970–1.376 ms |
| 9 MiB | 1 | 1.525–1.705 → 0.885–1.027 ms | 3.208–3.730 → 4.055–5.187 ms |
| 16 MiB | 1 | 2.721–2.742 → 1.676–1.916 ms | 8.269–9.006 → 7.650–9.064 ms |
| 64 MiB | 1 | 10.989–11.285 → 5.926–7.274 ms | 31.734–34.629 → 28.574–31.339 ms |
| 16 MiB | 2 | 3.388–3.418 → 3.116–3.424 ms | 9.205–10.740 → 9.183–10.099 ms |
| 16 MiB | 4 | 4.780–6.344 → 6.576–6.940 ms | 17.744–18.624 → 14.665–16.252 ms |

For 21 unchanged 16 MiB refreshes with one caller, the two workers spent 62.5–93.8 ms combined according to Windows thread CPU accounting. The caller and workers used 139–160 million cycles together, versus 114–115 million cycles for the serial caller. CPU time is quantized on this system. Cycle counts give a second view of total work, with frequency and scheduling variation.

The default pool reserved 4 MiB of virtual stack space and committed 188–204 KiB across both stacks by the end of each process. Private commit rose 136–148 KiB around the first 4 MiB parallel digest, which took 0.680–0.795 ms including pool setup. The 512 KiB test stacks saved 3 MiB of virtual space. Their final stack commit was 200–204 KiB and their initial process private commit increase was 132 KiB. The implementation retains the standard stack size.

The contention fixture uses independent calling threads. The normal submission path serializes preparation on the host submission lock. The synthetic matrix measures CPU work and native upload memory with generated committed, nonzero textures. It excludes upload commands, GPU execution, title memory contention and frame timing.

## Authorized title comparison

Use one diagnostics build with the parallel switch off and on in fresh processes. Set `VICTORY_TEXTURE_HASH_TIMING=1` in both. The normal frame-statistics interval reports untracked hashed and changed megabytes and hash and detile milliseconds per frame. Warm the shader and pipeline caches and match adapter, scene, input and measurement window. Record those aggregate values plus CPU preparation, GPU submission time, frame time, CPU utilization and private commit. Keep the two modes on the same build because the new lockfile changes the translator's cache identity. The single-caller unchanged fixture consistently gained wall time, while changed and contended cases were mixed. Keep the option off by default pending matched title evidence on changed volume, CPU use and frame time.
