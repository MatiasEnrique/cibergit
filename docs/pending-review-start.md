# Starting a pending review with a whole-file comment

This flow covers one case: the selected GitHub account has no pending review, and the user wants to save a whole-file comment in a new unsubmitted review. It does not change the existing one-write flow for a known pending review, line comments, immediate comments, submissions, or dismissals.

## Provider writes

GitHub's `DraftPullRequestReviewThread` input cannot represent a `FILE` subject. The app therefore sends two writes after one confirmation:

1. `addPullRequestReview` creates an empty `PENDING` review at the exact head commit. Its event, body, and threads are null.
2. `addPullRequestReviewThread` adds the whole-file comment to the exact review ID returned by the first write. It sends the provider path, body, `subjectType: FILE`, and no line or side.

The confirmation shows the selected account, repository, pull request, file, body, and commit. It warns that the empty review may remain if the second stage does not finish. Cancelling the confirmation sends no provider writes and keeps the draft.

## Absence proof

The create stage needs a fresh, complete, nonserialized absence witness. It binds the selected viewer and account to the host, repository, pull request node ID, open state, base, and head. The full selected-account pending set must be empty.

A missing, partial, or cached read is not absence. Neither is a response containing an unclassifiable pending row. A row with a missing author, invalid login, invalid review ID, or non-PENDING state prevents the new absence witness even though legacy pending-review import can still ignore rows owned by another account. The provider repeats this read while holding the existing per-target mutation authority and before saving `CreateInFlight`.

## Durable state

One private record tracks the whole attempt. It contains one frozen intent and one monotonically advancing stage. The record limit is 524,288 encoded bytes. Bodies are limited to 65,536 raw bytes, IDs to 1,024 bytes, and stored error text to 16,384 bytes. There is exactly one record per selected account, repository, and pull request.

The journal reuses the review interaction directory checks, bounded reader, atomic private writer, hashed path components, and per-target lock. It does not introduce another locking or filesystem scheme. Missing records mean no prior start. Corrupt records, future versions, identity mismatches, missing required fields, and invalid transitions fail closed and block competing target writes.

The create acknowledgement must echo the create operation ID and return a valid new review ID, selected author, `PENDING` state, null submission time, exact commit, pull request, and repository. The app saves `ReviewCreated` with that exact ID before it can prepare the file write. The file stage repeats the complete selected-account pending read and requires exactly that review at the frozen base and head.

Receipts contain only bounded typed IDs and state needed for recovery. They do not copy the request body into acknowledgement summaries.

## Stops, uncertainty, and restart

Restart never dispatches either provider write. A recovered `PreparedCreate` record can only be cancelled locally, which proves zero transport and releases the target. `CreateInFlight`, `ThreadInFlight`, and `Uncertain` stay frozen. The app does not find a replacement review or comment by body, path, time, or list order.

A durable `ReviewCreated` record is quiescent and closable. The user can continue only after a fresh read proves the same sole pending review and a new confirmation freezes the exact draft again. The user can also choose "Stop and keep pending review." That local action sends no cleanup write, preserves the review ID and draft history, revokes continuation for this attempt, and lets later ordinary actions start only from fresh existing-pending evidence.

If the provider's FILE acknowledgement IDs are durable but local draft reconciliation did not finish, restart offers a local-only finish. It never resends the FILE mutation.

Any failure after a provider dispatch is uncertain unless a typed response proves the write was not applied. This includes a failure while reading state after the dispatch and a failure to save a terminal record, even when the known transport count was zero. A failed second stage never deletes or submits the created review.

## UI lifetime and draft ownership

Confirmation reads the actual visible FILE text only while the shared input belongs to the exact tab and action. Any edit invalidates the old confirmation and keeps the latest text. Workspace, tab instance, selected account, repository, pull request, action, stage, operation ID, and generation fence every completion before it changes busy state, status, or the composer.

After the first write, an active tab must still own the shared input and show the frozen body before the app can start the second write. An intentionally inactive tab may continue only when its controller still holds the same durable draft, target, and body. Lost confirmation lifetime, changed account, target, body, or head leaves `ReviewCreated`; it does not authorize a later background thread write.

Success links and clears only the exact sent draft predecessor. Newer text, other drafts, pin state, pending input, submitted-review editors, and unrelated journal entries remain untouched.
