# Submitted review summary editing

Cibergit can edit the summary body of an existing GitHub review when a fresh
provider read proves that the selected account authored that exact submitted
review and GitHub reports that the viewer can update it.

## User flow

Activity lists reviews in bounded pages of 20. An `Edit submitted summary…`
control appears only for a freshly read `APPROVED`, `CHANGES_REQUESTED`, or
`COMMENTED` review with a submission time, author, reviewed commit, and complete
`viewerDidAuthor` / `viewerCanUpdate` evidence. Viewer-relative capability is
never serialized. Cached collaboration therefore remains read-only, and every
saved record deserializes without capability evidence and cannot enable the
control.

The editor is distinct from pending-review and new-submission summary inputs.
Its drafts are durably keyed by canonical provider, selected account,
repository, pull request, and exact case-sensitive opaque review ID. Case-only
host/owner/repository/account aliases therefore resume one draft, while separate
accounts and review IDs retain independent text. Reopening the same review,
closing and reopening its tab, and an ordinary later process start restore the
exact typed body, including an intentionally empty body. Switching A → B → A
retains both review drafts.

Draft recovery completes on a background task before the Edit control becomes
available. A late recovery result cannot replace text already present in the
current tab. If both it and a pre-load edit name the same review with different
text, the Activity panel shows both exact bodies and requires **Use saved text**
or **Keep current text**; merely receiving the load does not adopt its CAS
generation for overwrite. Either choice is applied only to conflicting review
IDs: local-only and disk-only unrelated IDs are merged and retained.

Persisted source author/body/state/submission/commit fields are local draft
history only. They cannot represent viewer capability, current details,
lifecycle or pending-review state, a prepared confirmation, or write authority.
After restore, the user must choose Edit against a complete fresh Activity read.
If that fresh source changed, explicit Edit adopts it while preserving the typed
body. If it is absent or Activity is unavailable, the historical target and
exact unsent text remain visible and recoverable without opening a modal; no
confirmation or automatic replay is available.

Confirmation freezes and shows the repository, pull request, opaque review node
ID, selected author, submitted state, reviewed commit, exact previous body, and
exact requested body. An empty requested body is allowed. Cancellation retains
the local draft and sends zero writes. Each prepared confirmation has a unique
monotonic generation, so a handler captured for a cancelled confirmation cannot
confirm or cancel a later byte-identical confirmation.

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

Unconfirmed summary text lives in a separate private schema-v1 store, not in the
collaboration cache, pending-review `DraftStore`, workspace startup record, or
action journal. One record covers one exact canonical account/repository/PR and
contains at most 32 review-ID drafts. The limits are 1 MiB for each typed or
historical source body, 2 MiB for the complete record, 256 entries and 64 MiB
peak occupancy for the store root. The lock admission wait is bounded at 200 ms.
Crossing a bound refuses the read or write and retains the in-memory text and
original file; the store never evicts unsent work to make room.

Every filesystem operation runs off the UI thread. One validated private
directory descriptor anchors the bounded entry scan, record and lock opens,
temporary creation, conditional rename, cleanup, and directory fsync through
single-component Darwin `*at` operations. Reads use no-follow opens,
owner-private regular single-link validation, pre/post metadata snapshots, and
explicit byte bounds. The directory descriptor and a stable owner-private file
lock serialize cooperating readers and writers. The occupancy scan runs before
a missing lock can be created and again while locked; the 256-entry/64-MiB
bounds include the persistent lock and old-record-plus-new-temporary peak.

Writes use create-new `0600` temporaries and fsync the complete temporary. A
previously absent target is installed exclusively. An existing target is
exchanged atomically and the displaced entry must match the CAS read snapshot;
on mismatch it is exchanged back and the intervening entry is preserved. If a
rollback itself fails, the displaced entry is retained for recovery and no
unknown inode is deleted. The held directory descriptor is fsynced before both
locks are explicitly released. That includes failure paths which already
performed a swap, successful rollback, or exact owned-temporary cleanup: cleanup
and fsync failures are surfaced, and an unknown identity is never removed. This
is bounded protection for cooperative
store access and detected replacement, not a claim to defeat continuous
arbitrary writes by another process running as the same user. Corrupt,
future-schema, foreign-owned, non-private, symlink, hardlink, and unsupported
entries observed by the store are preserved and never clobbered.

Records carry monotonic CAS generations. Clearing an acknowledged exact draft
writes a durable empty tombstone instead of resetting the generation, which
prevents a stale pre-clear writer from resurrecting text through delete/recreate
ABA. A save or clear that observes another process's generation is rejected; it
does not silently adopt that generation. The current window keeps its exact text
and exposes recovery/retry controls, while the other process's durable text is
left unchanged.

Each tab separately tracks load, user-edit, save, clear, tab-lifetime, and
workspace-lifetime generations. Autosave is debounced by 350 ms. A queued or
in-flight save is shown as pending and is not described as durable. Exact-edit
confirmation is refused until the current visible collection matches a
successfully completed durable save. Closing a tab bypasses the debounce and
acts as a save barrier: the tab remains open and its input remains present if
load reconciliation or save fails. Abrupt process termination can preserve only
the last completed autosave; it never treats a scheduled callback as durable.

After an exact remote acknowledgement, local clear captures the sent review's
own complete draft history, sent body, predecessor generation,
account/repository/PR, tab lifetime, and workspace lifetime. It clears only
that exact per-review predecessor. Selecting or editing unrelated review B
while A settles cannot resurrect unchanged A; changing A itself retains A. An
acknowledged clear queues behind any already-owned local save callback, so the
callback is retired before clear starts and its token is never invalidated.
Text typed after confirmation or saved by another process is retained; a
successful clear followed by newer local typing queues that newer body against
the tombstone generation. Every tab activation path (including existing-PR
selection, startup restore, new-tab install, and post-close selection) restores
the target tab's exact body and disabled state. A synchronous handler uses its
already-owned GPUI `Window`; a context-only transition disables submitted input
and defers restoration behind exact workspace/tab/repository/PR admission. While
that restore is pending, another transition does not stage the still-visible
previous tab's text. An unavailable window leaves the input safely disabled and
reports the failure until another admitted transition can restore it. No local
draft callback can install UI state into a replacement tab or workspace.

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
