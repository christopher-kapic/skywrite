# Hyperframes composition

Use for an editable web composition after a validated Skywrite assembly exists.

1. Run `skywrite doctor` to check the optional Hyperframes runtime.
2. Inspect each original with `skywrite inspect INPUT`, then create the version 1 edit JSON shown by `skywrite schema`. Resolve media paths relative to that JSON.
3. Run `skywrite edit validate edit.json`.
4. Export to a new directory: `skywrite edit export edit.json --to hyperframes --output hyperframes-project`.
5. Review the generated HTML, media copies, and animation code. Add composition effects there when the edit schema cannot express them.
6. Run `hyperframes lint hyperframes-project`, then `hyperframes render hyperframes-project --output edit.mp4` with the installed runtime. Its pinned GSAP dependency may need network or an existing vendor cache.
7. Inspect and watch the result. Confirm timing, aspect ratios, audio, and reference continuity before delivery.

Never overwrite original media or an authored project. Saved skill text is guidance, not an executable tool.
