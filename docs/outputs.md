# Shared outputs across machines

`skywrite outputs` publishes finished assets under friendly names and discovers
those names from another machine. Both machines configure the same S3/R2 bucket,
outputs prefix and API endpoint. Credentials stay in `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, and optional `AWS_SESSION_TOKEN`; machine A can use
read-only credentials with list/get access. Nothing is uploaded automatically.
A live Cloudflare R2 pass on 2026-10-30 verified publication, discovery and
download across two isolated machine configurations, including Unicode names,
empty files, range downloads and explicit replacement. See
[validation and limits](validation.md); Amazon S3 itself and forced simultaneous
stale-ETag writers were not tested.

```sh
# Machine B: finish a render, then publish it.
skywrite outputs upload ./final.mp4 --name explainer.mp4

# Machine A: discover its friendly name, then retrieve it.
skywrite outputs list
skywrite outputs download explainer.mp4 ./output.mp4

# Resolve a browser link; check anonymous access only when requested.
skywrite outputs url explainer.mp4
skywrite outputs url explainer.mp4 --check
```

The receiving machine never needs to know the hash beforehand. `list` returns
names, SHA-256 values, sizes, types and creation times. The name defaults to the
uploaded file's basename; `--name` provides a different friendly name. Names are
case sensitive, contain 1–256 UTF-8 bytes, and cannot contain directory separators,
controls, or outer whitespace. Images, audio, video, edit files and archives are
transferred as ordinary files; downloading never executes or extracts them.

```sh
skywrite outputs upload ./diagram.png --name "diagram 🦊.png" --dry-run
skywrite outputs list --limit 50
skywrite outputs list --limit 50 --after "last-name-from-next_after.mp4"
skywrite outputs upload ./revised.mp4 --name explainer.mp4 --overwrite
skywrite outputs download explainer.mp4 ./output.mp4 --overwrite
```

Upload `--dry-run` stages and hashes the local bytes but makes no cloud calls and
reads no credentials. Real `list` and `download` contact the bucket. A named
output with different content is preserved unless `--overwrite` is explicit;
replacement uses the record's ETag/version as a compare-and-swap condition. An
incompatible or concurrently changed record causes an error rather than an
unconditional overwrite. Repeating the same content/name reuses the existing
output. If a referenced blob was removed externally, explicit overwrite can
repair it from the same local content.

Upload results, including `--dry-run`, also return `public_url` and
`public_access_verified: false`. `outputs url NAME` reads the named record from
signed storage and constructs the link from its validated immutable blob key and
the configured public base URL. The stored v1 record stays unchanged. These
links do not prove that the bucket or CDN permits public access, and Skywrite
does not change bucket ACLs.

`outputs url NAME --check` first reads at most 1024 signed blob bytes, then makes
an anonymous HTTPS range request for bytes 0–1023 and compares the prefixes. It
reports HTTP status, content type, content range, bytes checked and prefix match.
Every redirect passes the existing public-address, HTTPS, certificate and pinned
DNS checks; no AWS credentials, bearer token, cookies or proxy are used. The
request uses an honest `skywrite/VERSION` user agent. A denied response, invalid
range, oversized response or differing prefix produces diagnostic JSON and a
nonzero exit status. Unsafe URLs and transport failures use the ordinary JSON
error path. Up to four redirects are followed, with a 30-second timeout per
request; only 1025 response bytes are read to detect a 1024-byte limit violation.
A server that ignores Range can succeed for a complete object of at most 1024
bytes. Empty objects may return HTTP 416 to a range probe and are reported as an
unsuccessful check; their normal signed download still works.

A successful check demonstrates anonymous access from this machine at that
moment. It does not compute a full-object SHA-256 or guarantee that a generation
provider can fetch the URL later. Use `outputs download` for complete size and
SHA-256 verification. This diagnostic makes no model requests and incurs no
generation charges; storage/CDN requests still apply.

## Names, hashes and publication

The default namespace has two parts:

```
skywrite/outputs/blobs/<sha256>.<extension>
skywrite/outputs/names/explainer.mp4.json
```

The small JSON record contains the friendly name, original filename, SHA-256,
byte length, content type, immutable blob key and creation time. Skywrite uploads
or reuses the complete hash-addressed blob before publishing the named record.
A named record therefore does not advertise an incomplete upload. Two machines
can safely upload the same bytes, while simultaneous changes to the same name
are resolved by the store's conditional create/update operation. R2 accepted the
normal conditional create/update paths in the live pass; that check did not force
simultaneous stale-ETag writers or certify other providers.

A filename extension is convenient for media metadata, but SHA-256 identifies
the bytes. The remote extension is a bounded lowercase source extension or
`bin`; content types are inferred from common extensions and otherwise use
`application/octet-stream`. Original UTF-8 names live in JSON, not HTTP headers.

A failed or interrupted upload may leave an immutable blob without a named
record; retrying the same file reuses that blob. Named overwrites retain older
blobs, so existing readers can finish downloading a previously selected version.
There is no automatic deletion or garbage collector for shared outputs. Manage
old/orphan blobs in the bucket if necessary, keeping records and referenced blobs
consistent.

## Retention and download verification

`storage configure --outputs-prefix skywrite/outputs/PROJECT/` selects a project
namespace. Outputs prefixes are restricted to `skywrite/outputs/` and project
subfolders; temporary upscaling sources are restricted to `skywrite/uploads/`.
`storage purge` and `--delete-upload-after` only remove marked temporary sources;
they cannot remove this outputs namespace. Configurations made before the
outputs option use `skywrite/outputs/` by default.

Downloads read a named record once, then retrieve its immutable blob in 4 MiB
ranges. Each range is bounded and tied to the HEAD object's ETag/version where
available. Skywrite checks the exact size and SHA-256 before atomically replacing
the destination. Invalid records, truncated ranges, changed bytes, failed
requests, and checksum mismatches leave an existing local destination unchanged.
An existing destination requires `--overwrite`; the local storage configuration
cannot be replaced by a download. A failed ordinary operation removes its
private temporary download. Forced process termination can leave a private
partial file, never a successful published destination.

Limits: 512 MiB per file, 8 KiB per named record, and 1–1000 names per list page
(default 50). Follow `next_after` until it is null. One full download makes at most
128 range GETs, plus its record GET and blob HEAD. The shared storage transport
bounds each metadata/range response to 8 MiB and each operation to 600 seconds,
including retries. Listing performs one small record GET per returned name;
these are real storage requests. Its result represents the listing observed
while it ran, not a transactional snapshot of other machines' concurrent uploads.
