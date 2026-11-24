# Translated GPU address-window probe

This opt-in synthetic fixture extends the shared read-only scratch arena. It maps a guest byte range to an independently allocated native root SRV and uses a separate native root UAV for output. CPU pointer, native GPU virtual address and guest address are distinct. Existing title execution uses its existing address translation and shadow path.

The original shader begins from a guest address crossing a 32-bit carry boundary, follows addresses loaded from the first window and validates the full byte width at each load. It drops a deliberately misaligned root UAV store. The CPU compares every output with the reference computation. The input contains a pointer exactly beyond the window, which returns zero when followed. The original shadow path runs the same shader and participates in the reference checks. The fixture fills the arena's remaining capacity and confirms that another allocation is refused; the shadow comparison shows the alternate backing path.

## Reproduction

Use the Agility SDK 1.620.x DLL directory and run each adapter in a fresh process.

~~~powershell
$env:VICTORY_SHARED_TEST_SDK = '<Agility SDK x64 runtime directory>'
foreach ($adapter in 'amd', 'nvidia', 'warp') {
    $env:VICTORY_SHARED_TEST_ADAPTER = $adapter
    cargo test --locked -p victory-gpu --features diagnostics pointer_chasing_through_translated_root_windows -- --ignored --nocapture
}
~~~

Set `VICTORY_SHARED_TEST_TIMING=1` and rerun for timings with validation off. The reference and bounds checks run in both modes. The debug layer and full GPU-based validation were enabled for the separate AMD Radeon Graphics, RTX 5070 Laptop and WARP correctness runs.

## Synthetic comparison

This debug build used a 1 MiB input and a 256 KiB output, with 65,536 threads. The one-hop workload reads the source sequentially. The eight-hop workload follows a strided pointer permutation. Times below are medians of five samples per path, after one warm-up pair and with alternating order. GPU time is the pair of direct-queue timestamps bracketing the dispatch. Submission includes CPU preparation, queue execution and the fence wait, excluding list recording and output checking. The shared path uses a 1 MiB logical slice of the 256 MiB arena. The shadow path uses 1 MiB each of title backing and native shadow, excluding allocator rounding and process residency. The output and timing storage are common.

| Adapter | Hops | CPU preparation, µs | GPU dispatch, µs | Submission, µs |
| --- | ---: | ---: | ---: | ---: |
| AMD | 1 | 167 → 23 | 35.12 → 28.60 | 609 → 470 |
| AMD | 8 | 186 → 27 | 86.36 → 82.43 | 701 → 546 |
| NVIDIA | 1 | 153 → 20 | 91.87 → 91.23 | 566 → 413 |
| NVIDIA | 8 | 142 → 22 | 548.26 → 527.84 | 1,008 → 851 |
| WARP | 1 | 133 → 18 | 244.70 → 279.30 | 544 → 472 |
| WARP | 8 | 140 → 24 | 902.30 → 862.60 | 1,284 → 1,111 |

The one-hop effective input rates on NVIDIA were approximately 11.4 and 11.5 GB/s. These divide 1 MiB of logical loads by the dispatch times, including address checks and output stores. The dependent test measures aggregate dispatch time across many threads, so individual load latency and bus throughput remain unmeasured. Physical residency under memory pressure, shared guest physical-page routing, remapping, and title translator support remain open. The test serializes CPU and GPU access via the existing direct queue fence and uses original HLSL. It does not exercise translated guest ISA or prove title rendering behavior.

## Translated GFX10 check

A separate original one-thread GFX10 program runs through IR, structure,
resource and I/O tracking, DXIL emission and validation. It performs
`global_load_dword` and `s_buffer_load_dword` from a root SRV, then
`buffer_store_dword` and `buffer_atomic_add` through the root UAV
implemented in [#1077](https://github.com/Marvisak/VIctory/pull/1077).
The native output uses the production root UAV binder and copyback.

The fixture checks both guest spans before binding and before dispatch.
The 64-byte guest ranges cross a low-word carry boundary and the bound
views start 16 bytes in. CPU tests reject underflow, overflow, misaligned
bases and access beyond the end of a range. The synthetic guest bases,
CPU pointers and native GPU addresses are distinct. The two input
backings contain different data, allowing readback to detect selection of
the wrong window.

Use the same Agility variables as above and make the DXIL validator
available. Leave `VICTORY_SHARED_TEST_TIMING` unset for this fixture.

~~~powershell
cargo test --locked -p victory-gpu --features diagnostics --lib root_window
foreach ($adapter in 'amd', 'nvidia', 'warp') {
    $env:VICTORY_SHARED_TEST_ADAPTER = $adapter
    cargo test --locked -p victory-gpu --features diagnostics --lib draw::gfx10_window_tests::emitted_gfx10_load_store_and_atomic_through_checked_windows -- --exact --ignored --nocapture --test-threads=1
    if ($LASTEXITCODE) { throw "GFX10 window failed on $adapter" }
}
~~~

AMD Radeon Graphics, NVIDIA RTX 5070 Laptop GPU, and WARP all returned
29, 43, 32 through the independent shadow and 121, 137, 126 through the
shared input. The values are the vector load, accumulated atomic result
and scalar load. All output guard words stayed unchanged with the debug
layer and full GPU-based validation active.

The fixture installs the shared native input address after the normal
root binder records the independent path. These checked offsets are a
fixture constraint. Arbitrary title pointers, bounds inside the emitted
shader, external scalar `s_load_dword` data, global writes, alias
remapping and production residency remain open. This check records no
timings and does not change the default title path.
