# Pending file-level comments

This slice adds a native whole-file comment flow for an exact pending GitHub review owned by the selected account. It does not decide what to do when that pending review does not exist.

## Target model

Published review targets are a real sum type: `Line(PublishedPosition)` or `File(PublishedFile)`. A file target contains the exact retained Full PR base/head, raw-safe local file key, provider-safe UTF-8 path, and rename identity. It never carries a fabricated line, range, or side.

The selected file must be proven identical to the retained canonical Full PR file at the same head. Patch text is not part of that proof, so empty, binary, and renamed files are eligible. A non-UTF-8 current path is rejected while its local draft remains available.

Provider reads preserve the explicit GitHub `subjectType` as `FILE`, `LINE`, or `Unknown`. Missing legacy/cached provenance becomes `Unknown`; absence of a line is not treated as evidence of a file subject. File discussions appear in Activity with a whole-file label and are never placed on an invented inline row. Unknown discussions remain read-only.

## Pending-only write

The initial complete provider read may expose a nonserialized freshness witness only when it proves all of the following:

- selected viewer, review author, account, repository, pull request, and immutable node IDs match;
- the pull request is open at the exact canonical base and head;
- exactly one complete selected-account review is pending and unsubmitted;
- that review targets the exact current head.

Opening confirmation freezes the full witness, exact file target, durable draft ID, and exact body. Workspace, tab lifetime, account/repository, pull request, action, and monotonically increasing confirmation generation fence handlers and asynchronous completions. Reusing the same visible values after cancel/reopen cannot revive an old confirmation.

The provider obtains the same per-PR mutation authority used by existing review journals, checks the durable state, and re-reads the complete pending review immediately before dispatch. It then sends one `addPullRequestReviewThread` mutation with the exact pending review ID, path, body, `subjectType: FILE`, and client mutation ID. No line, side, range, event, or pull-request-only fallback is sent. GitHub has no expected-head compare-and-swap for this mutation, so the confirmation discloses the remaining head-change race.

An acknowledgement is certain only when GitHub echoes the operation ID and returns distinct new thread/comment IDs plus exact `FILE` subject, path, body, selected author, parent pending review ID/state/commit, pull-request ID/number, and repository. Missing or mismatched fields and post-dispatch transport failures are uncertain. A newly created thread whose ID was lost cannot be reconstructed from body, path, time, or list order and is never replayed automatically.

## Durable drafts

File drafts use the existing private `DraftStore`, partitioned by selected account/repository/pull request, but remain distinct from line drafts. They survive restart and can be reopened by exact target identity. Acknowledgement clears or links only the exact sent draft when both its current target and body still equal the frozen request; edits made after dispatch and unrelated file drafts remain unsent. Save callbacks use draft identity and exact saved body, independently of confirmation generations.

If no existing selected-account pending review is observed, the UI reports that the pending-only action is unavailable, retains the draft, and sends zero writes. It does not post immediately, invent a line, or create a pending review as a hidden first write. Designing the no-pending case remains separate work.

## Verification boundary

Provider tests use a scripted `gh` transport and assert the exact GraphQL request/response tuple, fresh identity/head rejection, uncertain acknowledgement handling, durable no-replay behavior, and explicit subject parsing. Native smoke preparation may use public read-only GitHub data and a clearly labelled synthetic exact-pending witness, but it must stop at confirmation and record zero mutation transports. No physical-input claim follows from in-process handler capture.

The focused native scene is enabled with `CIBERGIT_SMOKE_PENDING_FILE=1`, alongside the existing `ui-smoke` feature and smoke environment. Use a disposable `CIBERGIT_DATA_DIR`, a dedicated `CIBERGIT_SMOKE_DIR`, `CIBERGIT_SMOKE_BACKGROUND=1`, and an explicit light or dark appearance. It writes `native-pending-file-confirmation.png` plus `native-pending-file-smoke.txt`, exercises open/save/confirmation/cancel/stale-token/target-switch/reopen handlers, and deliberately never invokes the final confirm handler.
