# Wave64 instance-culling reproducer

The fixture assembles an original GFX10 compute shader. All 64 lanes have a lane ID. The first 16 lanes enter a masked branch and copy the source lane's value into their UAV output. The other 48 retain the sentinel `7`. Reading lane 4 is the active-source control. Reading lane 40 requires the value of an EXEC-inactive source lane that initialized its value before divergence. The waterfall control groups lanes by `lane % 4` and expects `17 + lane % 4` per output word.

## Results

| Revision and adapter | Active lane 4 | Inactive lane 40 | Plain waterfall | Partial-mask waterfall |
| --- | --- | --- | --- | --- |
| main `d2a4bae`, AMD Radeon Graphics 32–64 lanes | 64/64 exact | 48/64 exact, lanes 0–15 read 0 instead of 40 | Translation refused | Translation refused |
| main, NVIDIA RTX 5070 Laptop 32 lanes | Wave64 PSO refused | Wave64 PSO refused | Translation refused | Translation refused |
| main, WARP 4 lanes | Wave64 PSO refused | Wave64 PSO refused | Translation refused | Translation refused |
| #439 `d2189ff`, AMD | 64/64 exact | 64/64 exact | 64/64 exact | Translation refused at `0x58` |
| #439, NVIDIA | 64/64 exact | 64/64 exact | 64/64 exact | 64/64 exact for the class output |
| #439, WARP | Wave64 translation refused | Wave64 translation refused | Wave64 translation refused | Wave64 translation refused |

Every executed shader passed DXIL validation. The AMD and NVIDIA executions used the D3D12 debug layer and GPU-based validation. An accepted translation or an expected capability refusal is reported separately from GPU execution. The partial-mask test checks the final class output. It does not separately observe the vector write inside the partial mask section.

An isolated build of #385 `a345127` reproduced the main diagnostics for all five waterfall forms. It still emits the divergent lane read. The fixture asks for the scalar lane mask as a number, which is separate from tracking all incoming edges of a lane-mask phi. #439 restores executable coverage for the plain waterfall and inactive-lane read on the measured AMD and NVIDIA adapters. [#385](https://github.com/Marvisak/VIctory/pull/385), [#439](https://github.com/Marvisak/VIctory/pull/439)

## Reproduction

With the original fixture commit on `d2a4bae`:

```powershell
cargo test --locked -p victory-shader wave64_culling_translation_controls_and_refusals
$env:VICTORY_CULL_AGILITY = 'C:/path/to/Agility/build/native/bin/x64'
$env:VICTORY_CULL_ADAPTER = 'amd' # nvidia or warp for capability checks
cargo test --locked -p victory-shader wave64_culling_readback -- --ignored --nocapture
```

The ignored native test asserts the CPU reference. Main intentionally fails that assertion on the measured AMD adapter for the inactive-source case. For #439 the local comparison fixture uses `Host::target()` with its target-aware `emit` API. Use the [exact #439 comparison branch](https://github.com/aycow/VIctory/tree/test/instance-culling-pr439) and run the same native command. Use separate Cargo target directories for each checkout.

## Producer and consumer scope

The tested readback observes the culling program's producer bytes directly. A later shader that treats zeros as no work would see all 16 selected instances missing on the measured main path. The next native fixture can extend `Host::dispatch` to accept a second pass and bind the producer UAV's exact resource as the consumer SRV after an unordered-access-to-shader-resource transition. The consumer can count visible entries and write to an independent UAV for readback. That would check the same-resource handoff without introducing a second D3D12 harness. The current fixture contains one dispatch, and title culling-to-mesh and lighting edges remain to be confirmed by aggregate tester traces. [Instance-culling report](https://github.com/Marvisak/VIctory/issues/71#issuecomment-7123577206), [follow-up](https://github.com/Marvisak/VIctory/issues/71#issuecomment-7123625627)

No proprietary shaders or title assets were used.
