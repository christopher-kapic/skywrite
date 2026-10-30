# Native typography and Google Fonts

`skywrite image render` uses the current original Takumi Rust renderer. No browser, JavaScript, or external Takumi command is needed. Geist is bundled with its OFL license. Additional local `.ttf`, `.otf`, `.woff`, or `.woff2` fonts can be loaded with repeatable `--font /path/to/font`.

Fetch an explicitly chosen Google Fonts family with repeatable `--google-font`:

```sh
skywrite image render --file title.html -o title.png \
  --google-font 'Inter:wght@400;700'
skywrite image render --file title.html -o title-offline.png \
  --google-font 'Inter:wght@400;700' --offline
```

Use `font-family:Inter` and the corresponding `font-weight` in the template. A bare family requests its default style, usually regular weight 400. CSS2 axis selectors are supported, including `Inter:wght@100..900` and `Crimson Pro:ital,wght@0,400;1,700`. Google validates available styles, sorted tuples, and family support. Request the styles you need; ranges can download many static faces for the native renderer.

Only the family/axis selector and `display=swap` are sent to Google's CSS2 endpoint. Skywrite never sends the HTML, its text, or a `text=` subsetting parameter to Google. Responses may reference only HTTPS assets on `fonts.gstatic.com/s/`; redirects are rejected. No OpenRouter key is used or sent. Explicit network access happens only on a requested family cache miss; `--offline` requires cached files and fails clearly otherwise.

Files are stored under the shared cache root (`~/.cache/skywrite` by default), in `fonts/google`. `--cache-dir`, `SKYWRITE_CACHE_DIR`, and `XDG_CACHE_HOME` follow the shared cache rules. Family manifests record the selector, source URL, and SHA-256 of every font. File hashes and byte counts are verified on reuse, including offline renders. Corrupt entries fail rather than being silently used; remove the affected family manifest/file to fetch it again. Cache contents are regeneratable. Google font provenance appears in the render JSON.

The native template supports inline `style` and Takumi `tw` utility attributes. These include Tailwind-style layout/color/typography utilities implemented by Takumi; this is not a complete Tailwind compiler or a browser. `<style>`, `<link>`, arbitrary external stylesheets, and remote image URLs are rejected. Google Fonts is an explicit font loader and does not enable those HTML features.

Up to eight Google selectors and sixteen total additional font files (local plus Google) are allowed. CSS responses are limited to 128 KiB, each font to 8 MiB, and all extra image/font resources to 32 MiB. Font fetches have a 15-second connection timeout, a 60-second per-request timeout, and a 120-second total network deadline. Request fewer weights/families if the file or resource budget is exceeded.

Google Fonts includes fonts under different licenses. Check each family's license before distributing font files. Skywrite records source URLs but does not automatically obtain or assert a license for every downloaded family. See the [Google Fonts CSS2 documentation](https://developers.google.com/fonts/docs/css2) for selectors and defaults.
