# Native pull request reactions

This slice supports GitHub reactions on the four pull request objects that implement GraphQL `Reactable`:

- the pull request;
- a submitted pull request review;
- a pull request discussion `IssueComment`;
- a `PullRequestReviewComment`, including line, file-level, outdated, and currently unplaced comments.

The available contents are `THUMBS_UP`, `THUMBS_DOWN`, `LAUGH`, `CONFUSED`, `HEART`, `HOORAY`, `ROCKET`, and `EYES`. A compact row appears on the PR Overview, on every paged Activity discussion and review card, on every inline review comment, and on every comment in the capped Activity-only file-level or unplaced thread list.

## Read authority

Reaction counts and the selected-viewer marker are historical snapshot data and may be serialized in the collaboration cache. The fresh capability is a separate nonserialized value. Cache loading therefore preserves useful counts but always makes the row read-only. Missing old fields, partial GraphQL responses, duplicate groups, unknown contents, incomplete nested comment pages, and bounded pagination failures remain Unknown; none is converted into mutation authority.

A fresh subject freezes the provider, host, selected account, owner, repository, PR number, provider-supplied opaque PR node ID, subject type and node ID, exact subject body, selected viewer node ID and login, reaction content, and—only for a review comment—the exact parent review node ID. The subject author does not establish selected-viewer authority and need not equal the viewer.

GitHub’s `viewerCanReact` is direct evidence for Add. Remove instead requires a complete targeted `reactions(content:)` read that identifies exactly one reaction whose user node ID and login both match the selected viewer. The connection is paged at 100 entries for at most 10 pages. Partial entries, contradictory `viewerHasReacted`, a missing cursor, a second own reaction, or exhaustion of that bound sends no mutation.

No PR state, current-head, submitted-review-state, or authorship condition is added. A narrower target is rejected only when the provider’s exact type, parent, identity, content, viewer, or capability evidence does not match.

## One click, one frozen operation

The clicked row freezes Add or Remove. A later read never turns it into the opposite operation.

1. A targeted read prepares the exact request. Add requires direct absence for the selected viewer/content and fresh `viewerCanReact`. Remove records the one exact own reaction ID.
2. The existing per-PR action journal saves the request and takes the shared review/action authority. If that initial durable save fails, transport receives zero writes.
3. While the journal authority is held, a second targeted read repeats the exact selected account, viewer, PR, type, subject, parent, body, content, and state checks. Remove must find the same frozen reaction ID. A disagreement is recorded as NotStarted and sends zero writes.
4. The provider sends exactly one GraphQL `addReaction` or `removeReaction`. There is no REST fallback, browser fallback, node-ID decoding, batch operation, automatic retry, or routine confirmation modal.
5. A complete acknowledgement must echo the operation ID and exact reaction ID, content, viewer, reactable type and ID, subject type and ID, PR/repository parent, review parent where applicable, exact subject body, and final selected-viewer presence state.

The UI disables remote mutation affordances while preparation or transport owns the shared per-PR authority. Local review drafts and the pinned comparison remain intact. Reaction completions carry the workspace lifetime, tab lifetime, selected account/repository, PR, subject, parent, content, frozen action, operation, attempt, and generation. A stale success or stale error cannot clear the busy state for a newer identical action.

## Uncertain outcomes and reconciliation

A transport loss, GraphQL error after dispatch, missing or wrong acknowledgement field, or failed terminal journal save leaves the attempt Uncertain and frozen. Startup, refresh, and reconnect do not replay it.

An add whose acknowledgement was lost has no known new reaction ID. A later present reaction can describe desired final-state convergence, but cannot identify this attempt, so the journal remains unresolved under the bounded policy.

For a remove, a later complete targeted read with exact selected viewer, subject, parent, content, and absence may record final-state convergence. This is not a claim about causation. If another own reaction ID is present, the client does not adopt it and never retries removal against it.

GitHub’s `removeReaction` input contains subject ID and content, not an expected reaction ID. A remove/re-add race can therefore occur after the final preflight. The returned reaction ID can reveal a mismatch only after transport; such a mismatch is Uncertain. This implementation does not claim server compare-and-swap and cannot guarantee that a replacement reaction is never touched.

Dismissal, pending-review creation, and Q65 policy are outside this slice.
