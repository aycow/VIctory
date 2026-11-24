# Concurrent copyback timing control

This branch ports the byte-run portion of Duff's #1024 (`3b2181a`) onto #387 and retains author attribution. The two concurrent CPU-write regressions passed on native Windows. This port keeps guarded `write_mapped` calls for changed bytes. The later direct-store shortcut in #1024 (`67125b3`) relies on an address-protection query and requires a separate proof that the mapping stays live through the stores.

Run the timing control on Windows with a working D3D12 adapter. CPU writes into native mapped memory model completed GPU writes. The timed interval starts after allocation, refresh, snapshot and the synthetic writes. Each case uses a new 4 MiB write-watched allocation. The reported median is from five runs after two warmups.

```powershell
cargo --config 'profile.dev.package.victory-gpu.opt-level=3' test -p victory-gpu --features diagnostics --lib copyback_dense_sparse_and_fragmented -- --ignored --nocapture
```

For the baseline, substitute only `Shadow::copy_back` from `489c9bf` in an isolated copy of this worktree and repeat the same command. This keeps the fixture, snapshot preparation and optimization level identical. The main results are CPU wall times on native Windows.

| Synthetic writes over 4 MiB | Bulk merge | Changed-byte runs |
|---|---:|---:|
| Every byte | 1.20 ms | 9.17 ms |
| One byte per 64 KiB | 1.30 ms | 4.68 ms |
| One byte per 32 bytes | 1.37 ms | 718.99 ms |

The fragmentation case makes one guarded write call per changed byte. This measures the candidate's snapshot comparison and CPU copyback, with no shader execution or title data. The changed-byte safety invariant and the cost of enforcing it are separate. This candidate remains an experiment pending a less expensive write path with a valid mapping-lifetime guarantee.

## Current #387 integration

The byte-precise candidate now branches from current #387 (`709647f`). An SSE2 comparison over the private buffers builds changed-byte runs, with a scalar implementation on other architectures. Write-watch guards are opened once per tracked page. A failed 64 KiB read retries at 4 KiB boundaries, and a partially readable page does not become clean in the shadow journal. The returned copy count includes only bytes reported written by Windows.

The timing control now also decommits one 4 KiB page in every fourth tracked page after snapshotting. It checks all 1,008 mapped destinations and reports the wall time for the 4 MiB logical range. The separate regression includes a read-only page and writes at both ends of its neighboring page.

| Synthetic writes over 4 MiB | Original changed-byte runs | SIMD merge and page fallback |
|---|---:|---:|
| Every byte | 6.713 ms | 2.217 ms |
| One byte per 64 KiB | 4.234 ms | 1.658 ms |
| One byte per 32 bytes | 784.636 ms | 634.196 ms |
| One byte per 4 KiB with decommitted holes | | 2.786 ms |

These are native Windows medians from consecutive runs of the previous byte-run implementation and the revised candidate, using five samples after two warmups per case and `opt-level=3` for the GPU crate. The established 19 buffer-ordering cases and four copyback cases passed independently on AMD Radeon Graphics, NVIDIA RTX 5070 Laptop, and WARP with full GPU-based validation.

The direct-store shortcut in #1024 requires a lifetime guarantee for the actual destination mapping. In a self-allocation test, `VirtualLock` succeeded and `VirtualFree(MEM_RELEASE)` still released the allocation. Protection queries are point-in-time observations. The fault handler in #1010 addresses inaccessible addresses. A writable replacement at the same address raises no fault. Existing checked kernel writes remain the fallback for generic guest addresses.
