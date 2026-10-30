# Native Takumi stills

Use HTML layouts for title cards, typography, overlays, and reference sheets.

1. Read `skywrite image render --help` for the available options.
2. Author local HTML with inline styles or upstream Takumi `tw` utilities. These utilities are not a full browser or a full Tailwind runtime; avoid assuming browser scripts or CSS behavior.
3. Use embedded Geist or an explicit local font: `skywrite image render --file title.html --output title.png --width 1280 --height 720 --font /absolute/path/font.ttf`.
4. Inspect the generated raster for wrapping, clipping, missing glyphs, and image placement. Adjust the HTML and render to a new output, or explicitly use `--overwrite`.
5. Use the still as an image clip in a validated Skywrite edit, or as a reviewed reference in a later creative workflow.

Rendering uses native upstream Takumi; it does not require the older standalone CLI. Read current help before using additional font sources. A still renderer does not create generative image/video assets by itself.
