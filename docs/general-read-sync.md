# Bounded general-read synchronization

This slice synchronizes two REST representations conditionally: pull-request rows used by sidebar enumeration and the single pull-request metadata object used by an open tab. It also serializes admitted general reads per selected GitHub account. It is not a whole-application throttle and it does not grant mutation authority.

## Request and nesting map

The application admits these top-level lanes through one `GeneralReadController` lifetime:

| Application lane | Top-level provider method | Nested reads |
| --- | --- | --- |
| Sidebar repository | `list_pull_requests_conditional` | REST list pages in order; one or more live GraphQL metadata hydrations for each accepted page |
| Open-tab metadata | `pull_request_conditional` | One REST PR object; live GraphQL metadata hydration |
| Open-tab details | `general_read(details + pending_review)` | Live GraphQL details pages, nested review/thread/comment pages, then the pending-review read; the local action journal is historical recovery, not provider authority |
| Open-tab lifecycle | `general_read(pr_lifecycle_snapshot + pr_lifecycle_choices)` | Live GraphQL lifecycle snapshot, then bounded REST choice pages for branches, labels, assignees, collaborators (reviewer users), and teams |

An admitted operation owns one account generation. A recognized rate response stops every later, not-yet-started nested read in that operation immediately. Later admissions for the same credential observe the retained floor; a different account remains independently admissible. Deferred automatic work is distinct from a coalesced explicit refresh, and the rotating follow-up cursor prevents a fast metadata lane from starving details, lifecycle, or later repositories on the same account.

## Implementation ownership

`app/read_sync.rs` owns each recurring read's lifecycle as well as account admission.
A tab or repository holds an opaque `RefreshLane`, allocated anew when that destination
is created. The controller keeps the lane in an active or deferred state. An explicit
request during an active read queues one replacement; periodic requests leave the current
observation eligible for installation.

The GPUI shell requests work with the current workspace, account-scoped repository,
resource and observation generation. It runs the provider task off the UI thread, then
returns the result with the destination's current context. `complete_refresh` distinguishes
an applied result, a discarded result that released its slot, and an unrelated completion
that must not trigger work in a replacement controller. Only an accepted successful result
can replace the conditional cache. Valid server pacing still survives a discarded result.

Metadata, details, lifecycle metadata and sidebar enumeration use this same interface.
The controller owns deferred intent and rotating resumption order; the shell supplies live
destinations and dispatches their feature-specific work. A temporarily closing tab remains
live but refuses new work. A forgotten destination cannot queue further reads, while an
already running request retains its account slot until completion. Actions jobs/logs retain
their explicit selection and cancellation protocol and share the same account admission.

Details-specific authority stays with participation: requesting a new details observation
revokes the prior pending-review absence capability even when account admission is deferred.
Mutations can invalidate the observation generation without releasing the running task's
slot. Successful completion still installs details and reconciles participation in the
existing order; scheduling never grants write authority.

The design uses a GPUI-independent state machine and single-owner scheduling. It adds no
actor runtime or generic workflow framework. Tests exercise coalescing, stale identities,
close/reopen, server floors, independent accounts and fair resumption through the read
module's interface with controlled time and results.

## Exact conditional representation

The private in-memory cache key contains all of:

- provider, host, selected account, owner, repository;
- method, path, canonical query;
- page and requested sidebar state, or pull-request number;
- `since`, `Accept`, and API version.

It contains no credential or token. A `304 Not Modified` is accepted only when the exact key retains the decoded body from a prior accepted `200`; otherwise the read is incomplete. Returned validators may update the exact retained entry, but no body is invented. GraphQL lifecycle, details, pending review, and metadata hydration are always live and never become ETag or representation-cache authority.

Pagination is staged as one chain. A REST page and all required GraphQL hydration must complete before that page contributes to returned payload authority. The cache is returned only after the complete chain succeeds; a missing body, identity mismatch, invalid body, byte limit, pagination limit, or nested failure returns no replacement cache.

The fixed cache limits are:

- 256 entries;
- 16 MiB total retained bytes;
- 2 MiB per retained response body, including bounded identity/validator accounting;
- 100 list items per page;
- the existing 100-page PR-list ceiling.

Eviction is deterministic and private to the controller lifetime. Offline disk observations remain historical presentation only and cannot satisfy a conditional `304` or authorize an action.

## Scheduling authority and callback fences

`X-Poll-Interval`, `Retry-After`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` are parsed by the hardened included-response parser before aggregate operation-byte rejection. Valid rate and poll floors are scheduling authority independent of payload/cache/UI acceptance. They survive an unrelated body or GraphQL error, a coalesced explicit follow-up, success reset, periodic/manual/focus/action triggers, and a stale matching-lifetime completion.

GraphQL additionally recognizes GitHub's HTTP-200 exhaustion form when the response has rate errors and `X-RateLimit-Remaining: 0`. Valid `Retry-After` has precedence. Ambiguous duplicate/conflicting rate metadata suspends the account instead of guessing a shorter deadline. Malformed metadata never clears an existing later floor.

Unix reset conversion uses the injected wall clock and a saturating remaining duration. A reset that expires before hydration or callback application becomes a zero-duration floor rather than permanent suspension. Wall-clock failure or monotonic `Instant` overflow suspends safely. A pre-existing later floor remains later.

Every callback first checks the workspace instance before touching a refresh gate, schedule, tab state, status, cache, or UI payload. A stale controller lifetime has no effect. Within the same controller lifetime, a stale account generation may only add a matching-account scheduling floor; it cannot install a body/cache, mutate UI, or release the current generation. Repository, tab-instance, PR, request-generation, and sequence fences remain additional requirements.

Pending submitted-draft close is also an admission barrier: general reads cannot start or resume for that tab while `close_after_save` is set. Deferral does not change the pinned comparison/session, line or FILE drafts, submitted-summary drafts, action journal, or shared composer inputs.

## Mutation split

General-read cache entries and scheduling admission are read-only observations. They are never reused as mutation preflight evidence. Existing mutation paths keep their own fresh, fail-closed final preflight, acknowledgement, durable draft/journal, exact pinned-session, and sequence guards. A mutation invalidating an active read causes that read to release only its owned scheduling slot; its payload and cache are rejected and the explicit post-effect refresh is coalesced.

No global scheduling scope is claimed. Extending this scheduler to mutation paths would change their authority model and is a separate contract: mutation admission, fresh preflight, dispatch, acknowledgement, and reconciliation would need to remain independently serialized and fail closed.

## Focused native evidence

The opt-in native harness waits for the ordinary real, read-only workspace preparation to settle, using an explicit public repository/account/PR. It then:

1. adds synthetic in-memory line, FILE, submitted-summary, and uncertain-journal witnesses;
2. installs a synthetic 90-second rate directive through the real root `GeneralReadController`;
3. invokes the real metadata refresh path and verifies that it defers before provider dispatch while retaining the explicit follow-up;
4. proves a different synthetic account is admitted while the primary account operation is active;
5. installs a separate synthetic poll directive and verifies later same-account deferral;
6. compares the serialized pinned session, canonical revision, recovery collections, journal, and shared input text exactly;
7. captures the actual Root metadata rate-deferral notice, clears only that already-proven synthetic queued follow-up for scene isolation, then directly presents the fixed poll and unavailable notices after their controller/mapping assertions; the latter two are presentation checks, not simulated provider callbacks;
8. runs a pure in-memory exact `200` then `304` fixture through the production single-PR retained-body resolver and rejects an orphan `304`.

The scheduling directives and conditional sequence are synthetic fixtures. The report explicitly states that no live `304` is claimed. The harness issues zero remote mutations and invokes no OS notification, prompt, focus, preview, PID, or global-setting path.

Build the dedicated smoke binary only in an exclusively assigned target directory:

```sh
CARGO_TARGET_DIR=/tmp/cibergit-general-read-sync-target \
  cargo build --locked --features ui-smoke --bin cibergit
```

Run it with a disposable data directory and an already configured account, for example:

```sh
CIBERGIT_DATA_DIR=/tmp/cibergit-general-sync-smoke-store \
CIBERGIT_SMOKE_DIR=/absolute/path/to/general-sync-evidence \
CIBERGIT_SMOKE_BACKGROUND=1 \
CIBERGIT_SMOKE_GENERAL_SYNC=1 \
CIBERGIT_SMOKE_APPEARANCE=light \
/tmp/cibergit-general-read-sync-target/aarch64-apple-darwin/debug/cibergit \
  --repo cli/cli --account ACCOUNT --pr 14130
```

The output contains one text report and three application-owned scene captures for the rate, poll, and unavailable notices. This establishes in-process controller/UI behavior only; it does not establish physical input, accessibility, acrylic identity, or live server `304` behavior.
