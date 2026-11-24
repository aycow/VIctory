# Checked copyback into guarded title memory

The parent of this change is #387 at 0899b7d. Its checked reader already preserves a Windows PAGE_GUARD page. Its checked writer attempted the entire destination and retried failures in smaller pieces. A failing WriteProcessMemory spent PAGE_GUARD in the controlled Windows fixture, then the retry changed the byte from 0x11 to 0x63.

The writer now checks each protection span before issuing a single kernel write. It skips stable guard and no-access spans and retains the mapped neighbors around a decommit. A kernel-confirmed byte count controls whether shadow copyback acknowledges the piece. An incomplete piece remains stale so a later shadow refresh can sample title memory again. #943 independently introduced a protection-region walker after an initial failed write. The initial attempt and piece retries in its 12cfd38 revision still allow the guarded-write sequence above. [#943](https://github.com/Marvisak/VIctory/pull/943) · [Windows guard-page behavior](https://learn.microsoft.com/en-us/windows/win32/memory/creating-guard-pages)

An address or protection can change after VirtualQuery. A guard installed in that interval can still be spent by the kernel, and the failed span receives at most one write attempt from this call. Protection spans are observations. An allocation or mapping lease must come from an owner that also governs release, remap and protection changes. The host DLL policy admits Windows memory operations outside the runtime's physical mapping hooks. Similarly, the existing write-watch registry can become stale if a title changes a block's protection outside those hooks. An earlier runtime watch operation can then change its protection before this checked writer runs. That ownership issue, repeated calls with overlapping spans, and a replacement mapping at the same address are outside this patch's per-call guarantee.

## Windows reproduction

Run the native CPU tests with the library's ordinary test build.

    cargo test --locked -p victory-gpu --lib host::copyback_protection_tests:: -- --nocapture --test-threads=1

The fixture changes protection at a known boundary, uses real pagefile-backed aliases, and checks that guarded bytes and their protection remain intact. It also checks sequential overlapping writes through the second alias, a read-only page, a decommitted page, continued writes on either side, and a guard installed after the region query. The late-guard case injects the transition immediately before invoking WriteProcessMemory and checks that no retry stores the payload. A separate test writes exact one-byte runs alongside a CPU writer touching a different byte of the same dword.

Run the matched writer comparison with an optimized GPU crate.

    cargo --config 'profile.dev.package.victory-gpu.opt-level=3' test --locked -p victory-gpu --lib mapped_write_cost -- --ignored --nocapture --test-threads=1

The timing covers checked writes into a 4 MiB logical span. It excludes destination allocation, fixture initialization, protection changes and verification. The test alternates the two implementations, performs two warmups and takes the median of seven samples. The old code is copied into the test as the negative control. The fragmented workload calls the writer for each one-byte change. Current #387 emits merged pieces up to 64 KiB.

| Synthetic Windows case | Baseline | Candidate |
|---|---:|---:|
| Dense, one range | 0.640 ms | 0.696 ms |
| Sparse, 64 one-byte runs | 0.680 ms | 1.272 ms |
| Fragmented, 131,072 one-byte runs | 1,240.609 ms | 2,346.589 ms |
| 16 decommitted pages, one range | 1.311 ms | 0.597 ms |

The scope is the existing bulk producer and its guard semantics. Calling this writer once per changed byte in a fragmented mask adds a separate query per run and gave the largest regression in the table. The generic mask consumer still needs a way to reconcile checked runs without repeating those queries or crossing a protected region.

## Shared reconciliation contract

Use the #1681 fixture to establish exact reported byte masks, adjacent unchanged bytes, boundary clipping and overflow. Decode only set byte bits and wait for the native comparison fence before reading or reusing metadata. Feed the completed masks into the consumer oracle on 8e748e1. The oracle's live CPU writer is in the additional bf3a5e2 control. Replay queue-ordered overlapping packets into one retained destination generation. Preserve disjoint CPU writes even inside the same dword. Check decommitted and read-only subranges and their mapped neighbors, reject stale-generation replay after remap at the same address, and reject an overflow record before applying its valid prefix. Then check explicit fallback with complete coverage.

The checked-writer fixture here adds PAGE_GUARD, virtual aliases and a late protection transition. #1681's comparison fixture waits its own fence, and the consumer oracle checks ordered replay of completed packets and rejects a replacement mapping with a fixture-owned generation. Production integration still needs a held-fence reset and resubmission run that retains metadata and derives a trustworthy generation from the mapping owner. This patch leaves #387's 64 KiB full-piece merge intact. The exact-mask consumer path is required to exercise byte-level preservation of a CPU write that races between that merge's read and write. [#1681](https://github.com/Marvisak/VIctory/pull/1681) · [NEW-21 coordination](https://github.com/Marvisak/VIctory/issues/1633#issuecomment-7144530797)
