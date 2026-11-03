# Personal styles, skills, feedback, and model defaults

Personal data lives in `~/.skywrite`: `styles/*.md`, `skills/*.md`, `nits/*.json`,
and `config.json`. Set `SKYWRITE_HOME` to an absolute directory to move it or
isolate a test. Regeneratable model/media caches have their own cache location;
copying this folder is sufficient to share these personal documents and feedback.

Styles are Markdown creative prompts. Skills are procedural Markdown instructions.
Reading or saving either never executes commands or starts a model request.

```sh
skywrite styles list
skywrite styles create warm-documentary --file style.md
skywrite styles read warm-documentary
skywrite styles update warm-documentary --content 'Warm, natural light; restrained cuts.'
skywrite skills list
skywrite skills read composing-tools
skywrite skills create my-workflow --file workflow.md
skywrite skills update my-workflow --file revised-workflow.md
skywrite nit 'Describe every tool frustration, confusing result, or missing affordance.'
```

`create` never replaces a personal file; `update` requires one to exist. Names
contain 1–64 ASCII letters, digits, underscores, or hyphens. Content is UTF-8,
limited to 256 KiB, with no NUL bytes. `--file` reads a regular file without changing
it; provide exactly one of `--file` and `--content`. Writes publish atomically.
List results are sorted by name and report origin, path, byte count, and SHA-256;
read results also include Markdown content. Invalid filenames not matching the
portable name rule are ignored by listing; matching symlinks and nonregular files
are errors rather than being followed.

Embedded skills cover Hyperframes, Remotion, FCPXML, FFmpeg, native Takumi,
synchronization, tool composition, and a character-reference workflow outline.
They are read-only and carry `origin: "builtin"` with a `builtin://skills/...`
path. Explicitly creating a personal skill of the same name shadows the builtin
and reports `origin: "user"`; updating a builtin without a personal file fails.
The files remain editable using an ordinary text editor. Generation and upscaling
steps in the outlines require checking the installed CLI's current capabilities.

Every frustration should be recorded explicitly with `skywrite nit DESCRIPTION`.
When Skywrite cannot decode or verify something (undecodable camera color labels,
an unsupported container, an NLE import you could not check), make the nit
actionable: include the raw `skywrite inspect` or command output, the camera or
application and the settings the operator confirmed, and the relevant source file
from https://github.com/christopher-kapic/skywrite (for example `src/color.rs` or
`src/export/xmeml.rs`).
Each call creates a unique JSON file, containing only the supplied description,
application version, schema version, and a Unix creation timestamp. Descriptions
must be nonempty, at most 8192 bytes, and contain no NUL. There is no automatic
error logging, environment or API-key capture, media collection, or upload. Review
your own text before sharing the nits folder with a friend.

Manage saved model choices explicitly:

```sh
skywrite models defaults
skywrite models set-default analysis provider/model-id
skywrite models unset-default analysis
```

Model defaults store model IDs only in `config.json` under `model_defaults`, for
`analysis`, `transcription`, `editing`, `image-generation`, `video-generation`,
`image-upscaling`, and `video-upscaling`. Credentials belong in their existing
environment variables, never here. Resolution is explicit `--model`, then the
task environment variable, then the saved default, then a documented fallback
where provided (transcription). Configuration setters serialize changes through
the stable `config.lock` inode, then atomically publish the config while retaining
unknown top-level fields. Malformed, oversized, and nonregular config files fail
without replacement. Readers see a complete old or new config. The lock is
released by closing its file descriptor, including on errors or process exit.
