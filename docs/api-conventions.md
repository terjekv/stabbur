# API conventions

The ordinary administrative API is rooted at `/api/v1`. The committed
[`openapi.json`](openapi.json) document is authoritative for paths, schemas, security, and content
types. `/api/v1/internal/workers` is a private runtime protocol and is not a public-client extension
surface.

## Authentication and secrets

Send a login session or API token as `Authorization: Bearer VALUE`. Login sessions are short-lived;
named API tokens are long-lived until expiry or revocation. Raw API and worker tokens are returned
exactly once. Responses, list operations, debug output, and errors never recover them.

Bootstrap is available only while the first-administrator secret is pending. The secret is stored
in an owner-only file and deleted after successful local bootstrap. API callers should avoid
putting secrets in shell history: use protected config/token files or an interactive client.

## Problems and request correlation

Errors use `application/problem+json` and contain HTTP status, stable `code`, safe `detail`, optional
field validation errors, and `request_id`. Every response also carries `x-request-id`. A caller may
supply an ASCII-valid `x-request-id` of at most 128 bytes; otherwise the server creates a UUIDv7.
Log the request ID, never the bearer token or full credential-bearing request.

Clients should branch on the HTTP status and stable code, not the human detail. Backend paths,
SQL errors, and worker cache layouts are intentionally hidden.

## Pagination

Collection endpoints use `limit` and an opaque `cursor`. The default limit is 50 and the maximum
is 200. Pass `next_cursor` unchanged to retrieve the next page. Do not parse or synthesize a cursor,
and do not assume cursor ordering is a public contract. Run and job collection items are lightweight
summaries; fetch the detail endpoint for run parameters/results or the complete builder envelope.

## Optimistic concurrency

Mutable resources return a strong revision ETag such as `"rev-3"`. Read the resource, retain its
ETag, and send it as `If-Match` on mutation. A stale value returns `412 stale_revision`. Channel
creation uses `If-Match: "rev-0"`; later moves use the current channel ETag. Clients must not retry
a stale mutation until they have reread the resource and reconciled intent.

## Idempotency

`POST /runs` and `POST /runs/{run}/cancel` require an `Idempotency-Key` of 1-255 bytes. Reuse the
same key only for the same semantic operation and authenticated principal. Run creation stores the
request identity and result atomically: an identical replay returns the original run, while a
different body with the same key returns a conflict.

One-time credential issuance cannot safely replay a raw secret. If the response to token or worker
credential creation is lost, revoke or rotate the possibly created credential instead of blindly
repeating the request.

## Artifacts and caching

Artifact identity is a lowercase SHA-256 digest. Uploads require an exact `Content-Length`; the
server streams the body to private storage, independently computes size and digest, fsyncs, and
atomically publishes only verified bytes.

Content GET and HEAD support one closed, open-ended, or suffix `Range`. Valid ranges return `206`,
`Content-Range`, exact `Content-Length`, and `Accept-Ranges: bytes`; invalid or multiple ranges
return `416`. Digest ETags are strong. `If-None-Match` can return `304`. Immutable content uses a
one-year public cache policy, so authorization boundaries must not rely on cache eviction.

## Resolver behavior

Resolve by software, channel, platform, architecture, and optional numeric macOS version. A pinned
channel variant wins. Otherwise exact architecture is preferred over universal, then explicit
resolution priority. The server returns `409 ambiguous_variant` rather than guessing. Resolution
also requires exactly one primary-installer artifact with a readable verified `present` location.

Versions are opaque non-empty strings. Clients must never sort them as semantic versions.

## Availability and operator views

Release `availability` is independent of lifecycle. `POST /releases/{release}/withdraw` requires the
current release revision and a bounded reason, appends withdrawal/audit history, and removes channel
bindings. It preserves immutable bytes, variants and the attained lifecycle, including `stable`.
Withdrawn releases cannot be promoted or resolved through a channel. Rebuilds never restore them.

Same-version builds compare immutable output separately from observations. Changed provenance with
the same outputs yields `evidence_changed`; unchanged evidence yields `no_change`; different output
bytes/variants yield `version_content_conflict`; failed checks yield `verification_failed`.
A successful check does not imply that an existing failed, rejected or withdrawn release is publishable.

`GET /software/{software}/status` returns channels with exact versions, latest/last-successful run
observations, enabled/outstanding target counts, next scheduled time and up to 200 blocked targets.
Blocked matching requires one enabled, non-draining worker seen within five minutes to satisfy all
capabilities. This is a diagnostic snapshot, not a reservation or promise of immediate execution.
`GET /operations/status` provides durable queue counts and oldest queued time.

`POST /workers/{worker}/drain` changes new-claim eligibility with an ETag. It preserves active leases
and credentials; disabling a worker invalidates active attempts. A target with an outstanding queued
or running run cannot enqueue another scheduled run. Missed intervals coalesce when it becomes idle.
Explicit manual triggers remain separately idempotent.

Recipe revision creation accepts `expected_sequence`. The adapter checks the expected next positive
sequence in the same write transaction as the append. A stale expectation receives HTTP 412.
