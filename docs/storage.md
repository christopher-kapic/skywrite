# Public S3/R2 video sources

A live Cloudflare R2 pass on 2026-10-30 verified upload/reuse, public URL bytes,
R2-backed upscaling, active-job protection, cleanup recovery, and scoped purge.
See [validation and limits](validation.md). Amazon S3 itself, other compatible
providers, maximum-size transfers, and forced concurrent stale-ETag races were
not certified. Configuration and dry-run commands do not verify a bucket.

OpenRouter's dedicated video upscaler requires an accessible HTTPS video URL.
Skywrite can upload a local MP4 to your explicitly configured public bucket,
reuse the same bytes later, and optionally remove the source after completion.
It uses Apache Arrow's maintained `object_store` Rust S3 backend directly;
users do not install the AWS CLI. Skywrite never creates a bucket or changes its
ACL/public access settings.

Configure the bucket's public access first. The signed S3 API endpoint and public
download base URL are separate settings. For R2, use its account API endpoint and
a custom domain or enabled R2 public development URL. Development URLs are rate
limited; see [Cloudflare public buckets](https://developers.cloudflare.com/r2/buckets/public-buckets/)
and [Rust S3 examples](https://developers.cloudflare.com/r2/examples/aws/aws-sdk-rust/).

```sh
export AWS_ACCESS_KEY_ID='your-bucket-scoped-access-key'
export AWS_SECRET_ACCESS_KEY='your-secret-key'
# Optional AWS temporary credentials: export AWS_SESSION_TOKEN='...'

skywrite storage configure \
  --endpoint https://ACCOUNT_ID.r2.cloudflarestorage.com \
  --bucket editor-media --region auto \
  --public-base-url https://media.example.com \
  --prefix skywrite/uploads/project-01/

skywrite storage status
skywrite storage upload camera.mp4 --dry-run
skywrite storage check-public 'skywrite/uploads/project-01/HASH.mp4'
skywrite upscale video camera.mp4 --upload \
  --model black-forest-labs/flux-video-upscale --factor 2 \
  --delete-upload-after
```

For AWS S3, supply the bucket's regional HTTPS S3 endpoint, AWS region, and public
HTTPS download base. The API uses path-style requests. Dotted bucket names and
nonstandard ports are unsupported. Public base URLs may include a path prefix;
do not put a bucket/path in the API endpoint. Use bucket-scoped object read,
write, delete and list credentials. Only the three explicit AWS credential
environment variables are used for storage operations; there is no IMDS,
container, profile or automatic credential discovery, and credentials are never
saved. Non-secret settings are stored in `~/.skywrite/storage.json`
(`SKYWRITE_HOME` overrides the personal directory). Changing settings does not
move or delete existing uploads.

`storage upload` returns a constructed `public_url` with
`public_access_verified: false`; successful signed storage access does not prove
anonymous public access. `storage check-public KEY` explicitly checks an existing
upload hash key beneath the configured uploads prefix, without reading AWS
credentials or contacting a model. It requests only bytes 0–1023 anonymously,
using `skywrite/VERSION`, and reports HTTP status, type, range, bytes checked and
whether the bounded anonymous read succeeded. It does not compare the bytes with
signed storage or certify their full SHA-256. Use `outputs url NAME --check` for a
signed-prefix comparison of a shared output.

The diagnostic follows at most four redirects. Each hop uses the same public
HTTPS, globally reachable IP, DNS pinning and TLS validation guard as public
generation downloads. It sends no AWS headers, bearer tokens or cookies and
disables proxies. Each request times out after 30 seconds and reads at most 1025
bytes to detect responses exceeding 1024 bytes. A server ignoring Range succeeds
only for a complete object no larger than that bound. Failed diagnostic results
are JSON with a nonzero exit status; a private bucket's HTTP 403 remains distinct
from signed-storage authorization and future provider fetch availability.

`--upload` explicitly makes the video's bytes available through your public
bucket. It conflicts with `--source-job` and `--source-url`.
`--delete-upload-after` requires `--upload`. Storage accepts playable MP4 videos
with known positive durations, at most 512 MiB. Transcode other containers with
FFmpeg first. HTTPS certificate validation remains enabled.

## Reuse and identity

A private staging copy is hashed while copying, so the uploaded bytes match the
SHA-256 even if the original is subsequently edited. Keys are
`skywrite/uploads/<optional-project>/<sha256>.mp4`, with `Content-Type: video/mp4`.
The extension helps media consumers; SHA-256 determines content identity.
Identical files with different names reuse the same object in the same prefix.

Only remote `NotFound` permits creation. Authorization/other HEAD failures are
errors. Existing objects must match length and Skywrite SHA-256/ownership metadata;
an ETag is not a SHA-256. Creation is conditional, and concurrent creators' results
are verified before use. Conflicting/unmarked hash keys are never overwritten.

The job records the non-secret bucket/key/hash and cleanup policy before upload
or paid submission. Only a `prepared` job uploads; completed, pending and uncertain
jobs do not upload again. Stable generation identity uses the source hash.
`storage upload --dry-run` and `upscale video --dry-run` perform no storage network
calls or credential reads. Model discovery can still use free catalog requests
unless `--offline` is supplied with cached metadata.

One operation holds a staged file up to 512 MiB; PUT can also hold those bounded
bytes in memory. Metadata responses are capped at 8 MiB, connect timeout is 15s,
request timeout 300s, retries at most two, and operation timeout 600s. These bounds
do not promise maximum-size uploads will finish on slow connections.

## Cleanup and purge

Sources remain for reuse by default. `--delete-upload-after` deletes only after a
completed remote job has a verified local result. Timeout, interruption,
uncertain submission, rejection and failed generation retain the upload.
Cleanup failures become `cleanup_status: failed` alongside the completed result;
the result remains available. `skywrite jobs resume JOB_ID` retries cleanup
without resubmitting a paid generation. A later explicit cleanup request for the
same upload can strengthen its original retention policy.

Other unfinished local jobs protect a shared object. Cleanup records
`deferred_active_job`; resume after the other job finishes. Lock order is job
lock then storage lock; active scans read atomic snapshots without other job
locks. Automatic cleanup also requires saved bucket settings to match the current
settings; restore the old settings to clean up an older profile.

```sh
# Read-only LIST/HEAD preview, no deletions.
skywrite storage purge --dry-run
# Delete marked Skywrite objects beneath the configured prefix.
skywrite storage purge
# Continue a bounded sweep using its returned next_after key.
skywrite storage purge --limit 500
skywrite storage purge --limit 500 --after 'skywrite/uploads/project-01/HASH.mp4'
```

Purge checks hash filenames and ownership metadata, including project subfolders,
and preserves unrelated objects. It skips active local jobs by default.
`--include-active` explicitly permits deleting their sources and can break those
jobs. Purge is not atomic: errors/deadlines can occur after some deletions. Results
report counts, inspection errors and truncation; follow `next_after` to continue.

Jobs on other computers are invisible. Use separate project prefixes and avoid
deleting sources another computer needs. Deletes remove current objects only;
versioned S3 buckets can retain older versions/delete markers, requiring bucket
lifecycle rules to reclaim their storage. Object deletion does not purge CDN caches.
