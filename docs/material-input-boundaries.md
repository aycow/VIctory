# Material input staging boundaries

This fixture uses an original compute producer and consumer on `rgarage` `a192692`. It compares the actual aligned and 16-byte-aligned root CBV paths, plus a native table CBV built with the scratch and deferred-fill sequence used by `State::tables`. No title shader or asset is involved. The table case models that staging sequence independently of a title pipeline's descriptor discovery.

The table shows the consumer readback across AMD Radeon Graphics, NVIDIA RTX 5070 Laptop GPU, and WARP with full GPU-based validation. The fixture also verifies that each producer wrote 29 back to title memory.

| Input route | CPU writes 43 after recording | GPU writes 29 in an earlier list of the same submission | Producer finishes in a separate submission |
|---|---:|---:|---:|
| Native shadow root CBV, 256-byte aligned | 43 | 29 | 29 |
| Scratch root CBV, 16-byte aligned | 43 | 11 | 29 |
| Scratch table CBV | 43 | 11 | 29 |

The scratch routes both prepare their fill before the native producer runs in this case. The aligned root route addresses the producer's live native shadow, so normal resource ordering lets it see 29. The test uses two closed lists for the same-submission case. A dependency within one native list has a different recording boundary.

## Reproduce

Use Windows, Agility SDK 1.620.x, and one fresh process for each adapter. Run with these environment variables in PowerShell.

```powershell
$env:VICTORY_MATERIAL_TEST_AGILITY = 'C:\path\to\Agility\build\native\bin\x64'
$env:VICTORY_MATERIAL_TEST_ADAPTER = 'amd'  # nvidia or warp
cargo test --locked -p victory-gpu --features diagnostics --lib material_constant_staging_observes_title_and_gpu_writes_at_distinct_boundaries -- --ignored --nocapture
```

## Aggregate title observations

On exact `a192692`, an authorized RTX 5080 tester sampled 301,500 nonzero-depth pixels across ten presents with material shader `63051e97862e7dad`. Zero samples had positive G after 8-bit export of the `mulops` diagnostic. G holds the colour input for red in that probe. This narrows the observed zero to that colour input at the diagnostic target's precision. The target has multiple writers and the PNG discards alpha, so the underlying float values and source remain unresolved. [Tester report](https://github.com/Marvisak/VIctory/issues/71#issuecomment-7141121420)

To distinguish the input paths, compare normal, `mulops`, and `texall` in the same covered in-world scene and preserve null/cleared/no-execution as separate states. Report counts of finite, exact-zero, nonzero values that fall below 8-bit resolution, and min/max for each probe channel before quantization where possible. In offline data-flow inspection, identify whether the red colour operand reads a sampled texture, a constant, or a buffer and share the categories and offsets only. For that input, report aggregate expected/bound descriptor kind, format, stride, null and upload status, and producer/consumer relation. Keep assets, shader binaries, desktop images, memory addresses and raw dumps local.
