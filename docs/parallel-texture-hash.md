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

## Authorized title comparison

Use one build with the switch off and on in fresh processes. Warm the shader and pipeline caches and match adapter, scene, input and measurement window. Record aggregate untracked hashed and changed bytes, hash and detile time, CPU preparation, GPU submission time, frame time, CPU utilization and private commit. Keep the two modes on the same build because the new lockfile changes the translator's cache identity. The two workers trade extra concurrent CPU use for lower measured hash latency. Their effect on title frame time and memory pressure is unverified.
