# Offline collaboration reading

cibergit keeps the last successfully fetched pull-request collaboration snapshot so Overview,
Activity, and Checks remain readable after an application restart when GitHub cannot be reached.
The inspector labels this content `Cached`, shows its age, identifies it as read-only, and retains
the provider's original completeness state and notice. If the inspector displays only the first
20 reviews, 30 mapped threads, or 40 checks from a larger cached payload, it discloses that display
limit. Top-level issue comments keep their existing paging.

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
