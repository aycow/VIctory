# Unwatched texture refresh cost

The fixture calls the existing texture staging refresh with generated RGBA8 tiles. It measures Windows CPU preparation. D3D12 supplies a mapped upload buffer. The test neither submits GPU commands nor includes GPU wait time.

```powershell
cargo test -p victory-gpu --release --features diagnostics --locked unwatched_tiled_texture_refresh_cost -- --ignored --nocapture --test-threads=1
```

Measurements are medians of 21 samples after three warmups. The changed case flips one byte of the last texel between refreshes and checks the resulting upload byte. The compare controls retain a full CPU copy of the tiled input and include updating that copy in the changed case. Times are milliseconds.

| Tile layout | Bytes | Mapping query | BLAKE3 | Unchanged refresh | Changed refresh | Exact compare | Compare and update |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 512 × 512 | 1,048,576 | 0.002 | 0.134 | 0.136 | 0.245 | 0.016 | 0.036 |
| 1920 × 1080 | 8,847,360 | 0.027 | 1.263 | 1.413 | 3.815 | 0.243 | 0.481 |
| 3840 × 2160 | 33,423,360 | 0.057 | 5.601 | 5.607 | 16.752 | 1.592 | 2.547 |
| 7680 × 4320 | 133,693,440 | 0.226 | 21.648 | 21.833 | 64.034 | 6.965 | 11.688 |

Hashing dominates the unchanged synthetic case. The exact-compare control requires memory in addition to the existing upload, and its timings exclude detiling. A production decision needs the title's hashed volume, change rate, and memory headroom.

In a diagnostics build on this branch, set `VICTORY_TEXTURE_HASH_TIMING=1` to add the aggregate hash duration and hashed and changed megabytes to each normal frame-statistics interval. Leave other copy options at their normal settings. Collect the same scene window with the same cache state, resolution and adapter. The timing switch preserves the existing content comparison and host GPU texture ownership decisions.
