# Translated material expression fixture

This synthetic pixel program samples an RGBA colour texture and an independent mask texture. The original GFX10 instructions compute `(1 - mask.r) * colour` using legacy floating-point multiply and export all four channels to a 2 × 2 float target. DXIL validation and D3D12 GPU-based validation are enabled. The CPU reference covers nonzero colour, fractional masks, zero texture samples, and the legacy zero-times-infinity case. Every pixel is checked bit for bit.

On Windows, build with `cargo test -p victory-shader --lib two_pixel_samples_feed_legacy_material_colour --no-run`. Place Agility SDK 1.620.2's x64 DLLs in a `D3D12` directory alongside the test executable. Set `VICTORY_DXIL` to the Windows SDK x64 `dxil.dll` if the validator is outside the standard installation. Set `VICTORY_TEST_MATERIAL_ADAPTER` to `amd`, `nvidia` or `warp`. Run the executable from its directory with `two_pixel_samples_feed_legacy_material_colour --ignored --nocapture`.

The five cases passed on AMD Radeon Graphics, NVIDIA RTX 5070 Laptop GPU, and WARP. This isolates translated arithmetic, sampling, and float exports with known inputs. The fixture uses synthetic inputs and does not exercise title descriptor provenance, GPU material producers, or the coherency of placed material buffers.
