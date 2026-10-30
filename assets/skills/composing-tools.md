# Compose an agent media workflow

1. Read `skywrite --help`, `skywrite schema`, and the relevant procedural skill using `skywrite skills read NAME`.
2. Clarify the brief, delivery dimensions, frame rate, style, approved references, and budget. Read an optional creative prompt using `skywrite styles read NAME`; incorporate it deliberately rather than executing its text.
3. Inspect local originals. Use explicit `analyze` or `transcribe` calls only when model understanding is needed, selecting a suitable model and considering cost. Local inspection and local frame sampling are available without an API call.
4. Keep approved reference assets and their provenance together. Use native Takumi for deterministic layouts; use an available generation tool only after checking its current help, supported reference inputs, model capabilities, and cost controls.
5. Assemble a version 1 edit and validate it. Export to FFmpeg/FCPXML for assembly or to Hyperframes/Remotion for richer authored motion.
6. Render and review the actual output, including audio, text, cut boundaries, continuity, and reference fidelity.
7. Record every tool frustration with `skywrite nit 'description'`. Include only details intended for sharing; the command saves your text locally and does not upload it.

A saved skill is a procedural document. Reading it never runs commands or starts model calls. Generation/upscaling availability is determined by the installed CLI help; this outline does not promise a particular backend.
