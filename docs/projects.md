# Durable generation projects

`skywrite project` (`batch` alias) runs a versioned JSON graph of image, video,
speech and music requests. It calls the same typed Rust adapters as `generate`.
It never executes shell commands from a manifest.

```sh
skywrite project schema
skywrite project init story.json
skywrite project validate story.json
skywrite project validate story.json --check-models --offline
skywrite project run story.json --concurrency 2
skywrite project status story.json
skywrite project resume story.json --concurrency 2
```

Init, structural validation and status make no network or paid requests. Checking
models uses catalog discovery and generation dry runs; `--offline` requires cached
catalogs. Run performs capability checks for every node before admitting any
request. Configure model defaults or supply each node's `model`. Selection is
frozen in the first run's private state, including nodes awaiting references.
A resumed pending node still uses its frozen model. Completed runs verify output
hashes without credentials or the network.

A manifest has `version: 1`, `output_root` and 1–256 `nodes`. Every node has `id`,
`output` and an `action` whose `type` is `image`, `video`, `speech` or `music`.
`project schema` emits a composable JSON Schema and an example. Optional common
fields are `model`, `take`, `depends_on`, and video-only `shot`.

All file paths are relative to the manifest directory. Outputs stay beneath its
`output_root`; parent/dot components and symlink paths are rejected. Output names
are distinct and must match their media type. Existing outputs are never replaced.
A file reference is `{"file":"assets/character.png"}`. A generated reference is
`{"node":"character"}` and implicitly adds a dependency on that image node.
Explicit `depends_on` edges express ordering without consuming the output.
Cycles, unknown nodes and local input/output aliases fail validation.

Local file references are images: each is limited to 16 MiB, with at most 256 MiB
across unique resolved paths. Structural validation checks these sizes before
reading image payloads. Each content-identity sweep hashes a resolved file once,
even when many nodes reference it; state retains the original relative path keys.
Actual hash reads also enforce the limits if a file grows after metadata checks.
The whole-project identity is rechecked before each first node admission, so a
256-node invocation performs at most 258 sweeps (initial state, existing-state
validation, and admissions), each bounded to 256 MiB plus one overflow sentinel
byte per attempted sweep. This is an I/O bound, not a monetary spending policy.
Generation's separate image decoding, pixel and combined-reference limits still
apply, including to image outputs used by dependent nodes.

Image actions accept `prompt`, `references`, `resolution`, `aspect_ratio`, `seed`.
Video actions accept those fields plus `first_frame`, `last_frame`, `duration`,
`audio`; frame inputs cannot be combined with general references. Speech accepts
`text`, `voice`, `instructions`, `speed`. Music accepts `prompt`, `instrumental`,
and one image `reference`. Model-specific support is validated by the existing
adapters. Dependency images use an internal placeholder only for capability
preflight; their real bytes determine the actual job ID once the dependency is
complete. No placeholder is ever submitted.

Concurrency defaults to 2 and is bounded to 1–8. A required failure stops new
admission while active workers drain; `--keep-going` permits independent nodes.
Video retrieval waits up to `--wait` seconds per node (default/max 1800); unfinished
nodes remain waiting and their dependents remain pending. Run/resume return the
aggregate JSON: inspect `complete`, `counts`, each node's `status` and `error`.
An incomplete project is a successful status response, not a claim that all jobs
completed.

State is atomically stored with owner-only access under
`$SKYWRITE_HOME/projects/<manifest-path-hash>` (default `~/.skywrite`). The stable
project lock covers scheduling and worker drainage. Lock order is project → job →
storage; job/storage operations never acquire a project lock. Source manifest
bytes and local input hashes must stay unchanged. A planned deterministic job ID
is durable before worker launch, and the adapter checks it again before any paid
submission. State records completed publication hashes.

After a crash, a started node only reads/resumes its existing job receipt. It never
submits again. A missing receipt remains uncertain, even when the process may have
stopped just before the first request. A pending node can start for the first time.
If a crash left a published output, recovery reproduces publication from the
completed job's verified artifact into private scratch storage and adopts the
existing output only when its bytes match. This covers image and audio conversion;
a different output is preserved and reported. Uncertainty needs
human reconciliation against provider activity (`jobs status` / `jobs resume`);
there are no automatic paid retakes. For an intentional retake, save a new manifest
path, new output paths, and an explicit changed `take`. Merely copying an unchanged
request preserves the generation engine's global deterministic job identity.

Optional `shot: {"size":"wide","angle":"eye level","motion":"locked"}` labels
support planning. Three occurrences of identical labels produce a repetition
warning. These are declared metadata, not visual assessment or a creative quality
guarantee; they never trigger regeneration.
