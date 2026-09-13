# Offline collaboration reading

cibergit keeps the last successfully fetched pull-request collaboration snapshot so Overview,
Activity, and Checks remain readable after an application restart when GitHub cannot be reached.
The inspector labels this content `Cached`, shows its age, identifies it as read-only, and retains
the provider's original completeness state and notice. If the inspector displays only the first
20 reviews, 30 mapped threads, or 40 checks from a larger cached payload, it discloses that display
limit. Top-level issue comments keep their existing paging.

## Saved workspace restart

An ordinary launch restores saved review tabs without `--repo`, `--account`, or `--pr`. The loader
keeps tab order and the active tab, resolves each tab against the exact saved repository and account,
and restores its validated comparison context. A closed pull request does not need to appear in the
cached open-pull-request list. Each new tab record therefore includes the last provider-observed
`PullRequest` value for presentation. That value may report a newer head than the pinned canonical
revision. The loader compares the saved canonical revision with the persisted review context and
does not advance it to the metadata head.

The presentation value does not populate fresh details, lifecycle state, pending-review linkage,
permissions, or write controls. Those states still require their normal fresh provider reads and
generation checks. Failed account discovery or provider reads do not prevent the saved session,
local drafts, and disposable collaboration cache from rendering.

Workspace schema v1 remains readable. A legacy tab without presentation metadata can use a pull
request from the exact-account sidebar cache. If that cache does not contain the pull request, the
app keeps the tab record and shows a notice. Opening the same repository, account, and pull request
successfully later records actual metadata and repairs the entry. The loader never invents a title,
revision, object ID, or capability.

Each tab's encoded presentation metadata is limited to 256 KiB, and `workspace.json` is limited to
4 MiB. The workspace reader stops after 4 MiB before parsing. Oversized, corrupt, future-version,
wrong-account, and mismatched-session data is refused and left in place. Workspace saves merge live
tabs into their prior slots, so an unavailable entry is not dropped when another tab changes.
Closing that exact tab removes its slot. Manual open, activation, or close actions also invalidate a
delayed startup batch, so a late callback cannot restore tabs over newer user navigation. An
explicit CLI destination may keep other saved tabs, but it wins the active selection after it
resolves. If the saved active tab is unavailable, the first restored tab becomes active while the
unavailable record remains saved.

This snapshot is disposable presentation data. It is separate from the workspace Store,
DraftStore, and ActionJournal. In particular, it does not:

- replace fresh `ReviewTab.details` or enter review-operation reconciliation;
- establish current permissions, lifecycle capability, merge readiness, or pending-review
  linkage;
- retire or acknowledge a draft or uncertain mutation;
- select the Since-review baseline or advance/change the pinned comparison;
- replay any remote operation when connectivity returns.

Cached issue comments never expose Edit or Delete. Cached discussion threads are shown only through
the existing exact canonical/current-anchor mapper in Activity and are not installed into the diff.
Local draft composition continues to load and save through DraftStore independently. Reconnect or
an explicit refresh starts provider reads only; retries and all remote mutations remain explicit.

## Record and ordering contract

Private schema-v1 records bind provider, host, selected account, repository owner/name, pull-request
number, the actual observation time in Unix milliseconds, a durable request reservation, and the
exact `PullRequestDetails` payload. Full object IDs inside that payload are observation metadata,
not a new head pin. The loader validates the outer identity, PR number, observation time, and every
nested provider coordinate before presenting anything. Future-schema, corrupt, oversized, wrong-
identity, symlink, and hardlink records are refused and preserved.

The filename is a SHA-256 digest of the complete identity tuple, so account and repository text is
not placed in filenames. A fixed 256-slot associative CAS control file stores a monotonic request
reservation for each active identity. A newer request for the same identity invalidates an older
permit even if the old payload is evicted, becomes absent again, or is replaced byte-for-byte.
When the table is full, replacing the oldest reservation conservatively invalidates that pending
writer. The record's predecessor SHA-256 digest is also checked under the lock. These rules prevent
late overwrite; they do not claim that local request order or timestamps prove provider snapshot
recency.

Both disk loads and writes run on the background executor. Load admission checks the workspace
lifetime, tab lifetime, selected account/repository/PR, and cache generation, and it never replaces
fresh details. Writes originate only after the existing fresh-details read lane admits the provider
response. A successful details read is cacheable even when the separate pending-review read fails;
pending linkage is never included.

## Filesystem and retention bounds

The collaboration cache uses these fixed limits:

- 4 MiB per encoded snapshot;
- 64 validated owned snapshot records;
- 64 MiB across measurable direct cache-root entries, including control, lock, and leftover temp
  files;
- 256 direct root entries and a 128 KiB CAS control record.

Reads use no-follow opens, private owner-only regular files, a single-link check, bounded reads, and
descriptor/path identity checks before and after the read. Writes hold a stable advisory lock with
a 200 ms bounded nonblocking acquisition, use an owner-private create-new temp, fsync the temp,
rename atomically, fsync the directory, and explicitly unlock. Root and lock descriptors remain
held and are post-validated. Failed temp creation never deletes a pre-existing collision; cleanup
is limited to the exact device/inode created by that write.

Retention runs under the lock and can remove only fully parsed, identity-valid, single-link,
owner-private schema-v1 snapshot files. It never removes Store, DraftStore, ActionJournal,
recovery, lock, control, unknown, corrupt, future, linked, or foreign files. Unknown directories or
special entries have unprovable occupancy and therefore make the write fail closed. Missing or
invalid CAS control in a non-empty cache also fails closed rather than resetting reservations. Any
cache failure leaves admitted fresh collaboration data and local draft persistence usable and is
reported honestly in the inspector.

## Native validation scope

The mixed-freshness offline collaboration smoke disables the details transport but still admits a
fresh lifecycle read. It checks that fresh lifecycle identity cannot make cached comments editable.
It is not a full provider-refusal test.

The ordinary-startup refusal smoke starts with only `--data-dir` and places a process-local `gh`
shim first on `PATH`. The shim logs and refuses every attempted `gh` command, including account
discovery, sidebar, metadata, details, and lifecycle reads. The smoke requires the saved tab, pinned
session, local draft, and cached collaboration view to render while fresh details, lifecycle, and
pending-review authority remain absent. This establishes independence from successful `gh` reads.
It does not claim a machine-wide network disconnect.
