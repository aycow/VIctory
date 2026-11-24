# Render-pass dependency diagnostics

Use this diagnosis when an intermediate texture shows geometry and the
normal presentation path gives a black image. The report follows texture
dependencies in host submission order, including across command lists.
It uses the existing submission fences and final-image probe.

## Collect a selected present

Build with `cargo build --profile diag -p victory --features diagnostics`.
Use the present count in the runtime's frame log to pick a frame. From the
same initial state, launch the same scene with the same settings and:

```powershell
$env:VICTORY_RENDER_DEPENDENCIES = '256'
$env:VICTORY_LOG = 'info,victory_gpu::dependencies=info'
# Run your normal diagnostics launch command.
```

Use an inclusive range such as `255-256` if you need adjacent records.
Tracking starts at process startup to find earlier producers. Detailed
reports cover the selected present numbers, and tracking stops after the
last selected present. Resource IDs and operation sequences are local to each
run. Match shader and pipeline hashes across runs. Match a texture within
one run by ID and use width, height and DXGI format to narrow down
candidates like those selected by PR #530. Same-size textures can have
different IDs and different producers.

Start with the `render presentation source` and its input access. Follow
`writer` to the operation with that sequence, then inspect its input
accesses. `on_path` marks operations reachable from the presentation
source, including candidate refused writers. `writer_frame` distinguishes
a same-frame write candidate from an earlier one. `refused` at the input
records the latest recognized producer refusal since that resource's last
admitted write. `state` on an access is the latest observed state at that
point in the trace. State events distinguish submission prelude barriers
from in-list transitions.

The frame summary's `reachable_refused` counts refusal operations in that
report that the graph reaches from the present. Each `render refused producer`
row groups those operations by pipeline, shader set and reason, with the count and earliest
sequence. Several output edges can reach one operation and it counts once.
The frame summary's `refused` count covers the whole captured interval.
Prior-frame edges may refer to sequences whose operation records are in an
earlier report.

For a matched #530 reproduction, share only the build revision, adapter
and driver, selected present number, whether the existing final probe
flagged black, the aggregate missing-writer/unresolved/refusal counts, and
the first suspect path segment. Include the pipeline and shader hashes,
resource ID and extent, prior or refused writer sequence and frame, and
the descriptor match field if it was reported. If you already tested #530,
include the selected dimensions/format and whether that view showed
geometry. Keep title assets, captures, shader binaries, and memory dumps local.
If also collecting GPU census measurements from #509, run that separately
with the same settings and report the matching present count and hashes.

For an `R8_UINT` (format 62) intermediate with coherent geometry and a
black final image, keep target preview off and select two nearby affected
presents after warming the same shader cache. Record the scene marker and
keep camera, settings, and the title build fixed. Follow the first observed
static reader of that texture ID, if any, to its outputs. Independently
walk back from the normal presentation source to the first unresolved
input. Report the producer and reader pass kinds, hashes, texture IDs and
extents, and counts for refusals and host-resource mismatches. If no
static reader appears, include the first candidate writer for the expected
color target and the trace's omitted count. Across matched runs, compare
hashes, extents and aggregate pixel statistics at the same threshold.

## Synthetic validation

The Windows fixture uses an original one-pixel producer, a forward copy,
and an HLSL compute consumer with an SRV and a UAV. It records the consumer
first and submits the producer list before it. It verifies the report from
the production submission path and reads back the offscreen presentation
source. The complete chain returns RGBA `[255, 0, 0, 255]` and a
same-present writer. Omitting the producer leaves the explicitly seeded
black texture, returns RGBA `[0, 0, 0, 255]`, and resolves to the preceding
present's writer. A separate graph test covers a resource with no observed
writer and a distinct refused-writer edge. The GPU test also records a
candidate refusal for the input and a second refusal for an unrelated
resource. The black case reports only the reachable refusal. The red
case's later copy replaces it.

Use the Windows Graphics Tools optional component and the runtime directory
from the D3D12 Agility SDK 1.620.x package. Run each selected adapter in a
fresh process. The fixture enables the debug layer and full GPU-based
validation and checks for validation warnings or errors.

```powershell
cargo test -p victory-gpu --features diagnostics lineage_distinguishes_absent_previous_and_refused_writers
cargo test -p victory-gpu --features diagnostics refused_producers_count_only_reachable_operations_once
$env:VICTORY_RENDER_DEPENDENCIES = '1-4'
$env:VICTORY_RENDER_TEST_AGILITY = 'C:\path\to\agility\build\native\bin\x64'
foreach ($vendor in 'amd', 'nvidia', 'warp') {
    $env:VICTORY_RENDER_TEST_ADAPTER = $vendor
    cargo test -p victory-gpu --features diagnostics dependencies::tests::gpu::synthetic_gpu -- --exact --ignored --nocapture
    if ($LASTEXITCODE -ne 0) { throw "Validation failed on $vendor" }
}
```

## Interpretation limits

- An edge is the latest visible command that could have written the whole
  texture. A draw, UAV access, resolve or partial copy can leave some or
  all bytes unchanged. A missing writer means the tracer saw no prior
  candidate. An older writer identifies observed reuse. Neither measures
  texture contents. The existing sparse black probe covers some final
  formats and can miss small non-black regions.
- Shader entries come from statically declared SRV/UAV bindings. Runtime
  descriptor-array indexing, direct/placed resources, buffers, aliasing,
  subresources, and concurrent shader accesses are outside this graph.
  `host_resource_matches` only compares the retained native resource in
  the observed slot with the title texture. It cannot validate the host
  descriptor's format, dimensions, generation, or eventual GPU lookup.
  A missing match field can mean unavailable host or title metadata.
- An earlier-frame writer is a sequence reference. Include that present
  in the requested range to get its operations. If a present never reaches
  the native copy, there is no report for that presentation source.
- Limits are 4,096 events per list, 16,384 nodes per present, 256 accesses
  per operation, and 32,768 distinct tracked resources per run. Treat a
  nonzero `omitted` count as incomplete coverage.
- The fixture supplies its own metadata at the normal command-list
  diagnostic hooks. It verifies native submission ordering and output, while
  title shader translation, dynamic descriptor allocation and the window
  presentation path require separate validation. Logging adds CPU work and
  the fixture measures correctness only.
