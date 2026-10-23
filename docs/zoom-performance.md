# Zoom rendering

Status: implemented; Intel Mac and browser performance validation pending. Updated 2026-10-23.

Continuous pinch/wheel zoom and slider drags retain the last raster scale for 120 ms after
the last change. Page geometry, selection and hit testing follow the displayed scale at once.
Releasing the slider or using a discrete zoom command ends the preview interval. Read Mode
and Web Layout retain their committed layout during continuous input, then reflow at rest.

Native scale refinement uses one long-lived coordinator per canvas and a retained
single-threaded render context separate from foreground rendering. Typing and other content
changes still render synchronously; foreground and background renders cannot create competing
full rayon pools. Prepared page drawing commands are reused across refinement tiles.
Only one job is admitted at a time; a completed result is accepted only for its document,
revision, page content, scale and tile dimensions. Document changes do not spawn overlapping
replacement workers. A running obsolete job finishes but cannot replace current content.

Jobs are limited to 2,097,152 pixels (8 MiB RGBA). Large normal pages now use visible horizontal
tiles too, with tile height reduced to fit that limit. At most one previous tile resolution is
retained for preview. The texture cache is capped at 64 MiB; normal upload passes use a 2M-pixel
budget and stop adding work after 4 ms. A first atomic render/upload can exceed that time, and
forced content-edit and object-drag feedback can exceed the ordinary per-frame budget so an
edit never leaves a partially updated page while its remaining visible tiles wait.

These are pixel/cache limits, not a process-memory cap: document snapshots, decoded-media
caches, prepared draw lists and renderer scratch remain separately allocated. A result and
its egui conversion can coexist with the next admitted render. The renderer's existing media
and input limits still apply. WASM retains the preview policy and reuses prepared commands,
but renders bounded tiles synchronously; browser-worker refinement is not implemented.

## Diagnostics and repeatable checks

`ui.inspect` exposes `perf.zoom`: foreground job count, submitted/completed/discarded refinement
counts, preview frames, pixels uploaded, texture-cache bytes, page preparation time, refinement
raster time (including drawing/decoding), image conversion time and texture submission time.
Submission time does not measure GPU completion or input-to-presentation latency.

Run focused checks:

```sh
cargo test -p wordcraft-ui-egui zoom --lib
cargo test -p wordcraft-render prepared_refinement --lib
cargo xtask ci
```

For a headless visual/performance pass, use `ui_shot` with a JSON-lines script containing
`ui.inspect`, repeated `ui.wheel` requests with `cmd: true`, `wait` for final refinement, and
screenshots before/during/after the gesture. Set `UI_SHOT_FULL=1` for complete diagnostics.
Use the same sample, viewport and 2× DPR for comparisons. The warm-pinch regression checks
that cached Print Layout zoom neither rasterizes/uploads pixels nor repaginates, while edits
still produce fresh pixels. Additional tests cover pointer anchoring, reduced motion,
Read Mode reflow, byte admission, stale results and large glyphs crossing tile boundaries.

## Remaining hardware gates

Local validation on 2026-10-23: the Windows workspace test run passed 3,897 tests (four
ignored), all 16 configured WASM crate checks passed, and formatting and diff checks passed.
Headless preview/final screenshots were inspected at 143% zoom and 2× DPR. During the
12-event zoom sequence, uploaded pixels and foreground render jobs stayed unchanged; final
refinement completed afterward. This is a functional check, not a hardware latency benchmark.
Existing Clippy blockers were corrected with behavior-preserving boolean/match simplifications;
the strict workspace lint check now passes. At the owner's request, the complete CI rerun
was stopped during test compilation; the successful workspace/WASM results above predate
those lint-only changes.
An Intel macOS release build and DMG packaging also passed, including strict signature checks
of the app inside the DMG and an installed copy. The local test package is ad-hoc signed,
not notarized; interactive Intel Mac performance measurements remain pending.

Measure release builds on a named Intel Retina Mac, Apple Silicon, Windows and WASM using
text, image and table-heavy documents, long Web pages, rapid reversals and high zoom. Record
frame and end-to-end latency, full job duration, sharpness delay, CPU and process memory.
The proposed p95 16.7 ms frame and 250 ms sharpness targets are not established by unit tests.
The Windows development environment cannot validate Intel Mac thermal or graphics behavior.
Horizontal tiles still scan prepared page commands; media misses and long pages may require
region indexing, smaller jobs or browser workers after profiling. No rendering-quality option
(including macOS text darkening) has been disabled.

## Revision history

| Date | Change |
| --- | --- |
| 2026-10-23 | Initial implementation and remaining validation limits. |
