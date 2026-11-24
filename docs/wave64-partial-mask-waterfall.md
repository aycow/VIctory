# Partial-mask Wave64 waterfall

The per-lane translation can reach a scalar read of saved EXEC from a
block that only some of those lanes reach. That block cannot reproduce
the mask with a native ballot. On hardware capable of Wave64, a
Wave64 compute group of exactly 64 threads with a recognized whole-wave
section can instead use the existing console-flow lowering. It keeps the
whole wave at each block and applies EXEC to the vector operations.
Existing split-wave selection on Wave32 hardware stays in use. Tests
cover the literal partial-mask case and a case whose class sums observe
the actual result of the section.

The fixtures use original GFX10, two groups of 64 lanes, and a four-class
waterfall. The first variant returns `17 + lane % 4`. The second sums
the lane IDs in each class and returns `480 + 16 * (lane % 4)`. Both
outputs must match in every lane. The first test fails translation in
#439 at `901eef9` before the change.

## Reproduce

```powershell
cargo test --locked -p victory-shader waterfall -- --nocapture
$env:VICTORY_CULL_AGILITY = 'C:\path\to\agility\build\native\bin\x64'
cargo test --locked -p victory-shader partial_waterfall_with_gpu_validation -- --ignored --nocapture --test-threads=1
```

AMD Radeon Graphics and NVIDIA RTX 5070 Laptop returned 128 matching
words for both variants with Agility SDK 1.620.2, the debug layer and
full GPU-based validation. The WARP adapter reported a wave size below
32, and the test verified that the programs were refused on that target.

These checks cover the translated producer output. The title's four
reported culling shaders, the runtime structured descriptors they use,
and downstream mesh, G-buffer, and lighting results require a separate
matched title run. Keep shader binaries and title assets local.
