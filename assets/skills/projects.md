# Declarative generation projects

Use `skywrite project schema` for the complete JSON contract and example. Save a
version 1 manifest with `output_root` and typed image/video/speech/music `nodes`.
Generate character images first and connect video `first_frame` / `references`
with `{"node":"character"}`; dependency edges are explicit and implicit references
wait for verified outputs. Choose varied shot size/angle/motion labels, then
review actual visuals. Repeated labels warn after three occurrences.

Validate before run. `project validate FILE` is offline structural validation;
`--check-models --offline` additionally checks cached capabilities. `project run
FILE --concurrency 2` freezes models and persists deterministic IDs before requests.
Use `project status FILE` and `project resume FILE` to aggregate/recover jobs.
Inspect `complete` and node errors; incomplete aggregate output is not success for
every node. Never delete state to retry uncertainty. Resume never repeats a started
submission. Reconcile uncertain jobs against provider activity before an explicit
new take/new manifest/new output. No automatic paid regeneration.

Paths are manifest-relative, outputs must remain beneath the declared output root,
and existing outputs and local inputs are preserved. Keep manifest bytes and source
input bytes unchanged throughout a run. Refer to docs/projects.md for recovery,
concurrency and lock behavior.
