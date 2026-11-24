# Sparse register enumeration in shader IR construction

The SSA builder enumerates live-in and live-out registers per basic block. The previous iterator visited all 385 possible registers on every scan. Iterating the set bits of each word preserves register order and skips inactive positions.

## Synthetic comparison

Both programs contain 20,488 original GFX10 words. The arithmetic fixture has one IR block and the control-flow fixture has 12,289. The fixtures are at `crates/victory-shader/src/emit/cfg_size_probe.rs`. Build with the shader crate optimized and run the ignored offline fixture on both `35a870b` and this revision.

```powershell
cargo --config 'profile.dev.package.victory-shader.opt-level=3' test -p victory-shader --lib compare_live_cfg_join_and_arithmetic_size_scaling -- --ignored --nocapture
```

The recorded measurement used an isolated copy of FunWithFlagsDev's v2 profiler with matching timers around IR subphases. It alternated baseline and candidate order for seven pairs of processes, collected five samples after one warmup per process, and excluded process startup and driver compilation. Total IR is the median of the seven per-process medians. SSA is the median across 35 samples and includes trivial-phi cleanup.

| Synthetic program | Total IR, baseline → candidate | SSA, baseline → candidate | DXIL bytes |
|---|---:|---:|---:|
| Live control flow | 46.58 → 38.50 ms | 21.52 → 13.15 ms | 180,320 |
| Straight arithmetic | 15.28 → 16.69 ms | 0.42 → 0.38 ms | 119,124 |

Emitted DXIL fingerprints matched for each program across all runs. The measured improvement applies to sparse live sets in this control-flow fixture. The arithmetic control's total IR time was higher with the candidate, and these experiments establish neither a general compile-time gain nor any GPU or title speedup. Structurization is unchanged.
