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
