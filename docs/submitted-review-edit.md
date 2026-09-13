# Submitted review summary editing

Cibergit can edit the summary body of an existing GitHub review when a fresh
provider read proves that the selected account authored that exact submitted
review and GitHub reports that the viewer can update it.

## User flow

Activity lists reviews in bounded pages of 20. An `Edit submitted summary…`
control appears only for a freshly read `APPROVED`, `CHANGES_REQUESTED`, or
`COMMENTED` review with a submission time, author, reviewed commit, and complete
`viewerDidAuthor` / `viewerCanUpdate` evidence. Cached collaboration remains
read-only; old saved records deserialize without capability evidence and cannot
enable the control.

The editor is distinct from pending-review and new-submission summary inputs.
Its text is retained per tab and per selected review while navigating. If a
refresh changes or removes the source review, the typed text remains visible or
is disclosed as retained, but confirmation is refused until editing restarts
from fresh evidence.

Confirmation freezes and shows the repository, pull request, opaque review node
ID, selected author, submitted state, reviewed commit, exact previous body, and
exact requested body. An empty requested body is allowed. Cancellation retains
the local draft and sends zero writes.

GitHub rechecks the exact review before saving, but another edit can happen
between that read and the write because the API has no atomic previous-body
condition. A review can refer to a commit older than the pull request head;
editing its summary does not advance or otherwise change Cibergit's immutable
displayed comparison, and it does not change the original review event.

## Provider and acknowledgement contract

Dispatch performs a fresh exact-node read and rejects before mutation unless all
of these still match the frozen request:

- provider coordinates and parent repository / pull request;
- selected credential viewer, review author, and `viewerDidAuthor`;
- complete `viewerCanUpdate` capability evidence;
- submitted time and one of the three editable submitted states;
- exact previous body, state, author, and reviewed commit.

The write is one GraphQL `updatePullRequestReview` call using the opaque review
node ID, requested body, and local operation ID. It is acknowledged only when
the response echoes that operation ID and the exact review ID, new body,
submitted state, author, commit, parent pull request, and repository. Missing,
malformed, partial, or mismatching post-dispatch responses are uncertain, never
safe failures.

## Durable recovery

The existing per-pull-request auxiliary journal is written before transport and
holds target authority until a terminal record is durable. It records the exact
old and new bodies plus all frozen target fields. At most one unresolved target
mutation is admitted; restart does not replay it. A terminal journal write
failure after transport remains uncertain.

Read-only reconciliation can resolve an exact known review ID only when its
current coordinates, body, submitted state, author, and commit match the frozen
requested final state. This proves that the desired state is currently
satisfied, not that the local attempt caused it. A later mismatch cannot prove
the attempt was not applied because the review may have changed again.

File-level review comments, review dismissal, and Q65 policy remain outside this
slice. There is no browser fallback and no live-write validation workflow.
