# Application architecture

The app keeps GPUI scheduling, widgets, focus, and rendering in the native shell. Modules own the protocols that determine whether work can start, what it can change, and whether a delayed result still belongs to the current state.

## Owners and interfaces

| Module | Owns | What the shell does |
| --- | --- | --- |
| `app::read_sync::GeneralReadController` | Account pacing, refresh admission, coalescing, deferred work, completion identity, and cache acceptance | Schedules admitted reads and presents accepted results |
| `review::ReviewSession` and `app::diff_pane` | Pinned comparisons, lazy patch identity, row construction, metrics, scroll, cursor, file spans, folding, and virtualization state | Loads requested patches and renders the comparison |
| `app::review_interactions` | Frozen participation jobs, durable-state admission, write sequencing, pending-review stages, and operation-specific reconciliation | Schedules jobs, supplies provider execution, and presents outcomes |
| `app::submitted_review_drafts::SubmittedSummaryEditor` | Submitted draft load/save/clear ordering, exact receipts, coalesced text, recovery, and close decisions | Supplies current text, schedules disk jobs, and manages focus |
| `app::workspace_save::WorkspacePersistence` | Navigation/session save ordering, close/reopen reads, failure stops, and startup restore admission | Schedules disk work and installs admitted tabs |
| `app::local_workspace::operation_lifecycle::OperationLifecycle` | Checkout admission, frozen local/rebase intent, authoritative observations, completion identity, and recovery gating | Executes existing Git/rebase operations and renders their state |

## Patterns in use

The functional core and imperative shell split places state transitions behind interfaces that ordinary tests can call. GPUI continues to use its own executor. There is no separate actor runtime or global event bus.

Explicit state machines represent refresh work and checkout operations. Opaque request and dispatch tokens bind completion to its issuing owner, selection, or checkout. Runtime checks still validate external Git and GitHub state immediately before effects. A Rust type cannot prove that a remote branch has stayed unchanged.

Prepared commands capture the exact operation the user confirmed. Durable journals record admission before dispatch, and reconciliation uses evidence specific to that operation. A timeout remains uncertain. Reading a matching final state does not automatically prove which attempt caused it, and the app never replays an uncertain mutation automatically. Creating a pending review and creating its FILE thread remain separate durable stages.

Immutable `Arc<Comparison>` snapshots share patch data. Cloning a review session preserves its progress but gives the clone an independent lazy-reader lifetime. PR display comparisons and canonical comment coordinates remain separate. History and Stack remain read-only.

Concrete adapters keep proven implementations in place: `Store`, `DraftStore`, `ReviewStateAuthority`, `ActionJournal`, `LocalGit`, `RebaseStore`, and `WorktreeManager`. The refactor adds no generic persistence framework, speculative provider abstraction, dependency, or storage format migration.

## Delayed work and recovery

A completion can change state only while its owner still accepts the corresponding request. A stale selected-patch error cannot change the newer display's loading state, even if the same task can still hydrate its canonical comparison. Local operation transitions retire both observation tickets and their pending flags, so rejected reads cannot leave controls permanently blocked.

Participation sequencing lives above individual tab controllers. Closing and reopening a controller therefore cannot reset the write order. Submitted-draft clear completion discards snapshots queued before its tombstone and rebuilds any successor from current text. This preserves newer typing without allowing an old save to resurrect cleared text.

Workspace session reads share the write owner. An immediate reopen can drain its queued final save before reading, even if the save worker has not started. History, the PR browser, and Settings count as explicit navigation and cancel delayed startup restore. Unavailable saved tab slots survive unless explicitly closed. See [workspace persistence](workspace-persistence.md) for the detailed ordering rules.

Disposable collaboration caches keep their own retention and compare-and-swap rules. They do not share the durable participation or workspace protocol, and cached collaboration data grants no mutation authority.

## Validation and remaining shell code

Lifecycle tests cover stale completion, reader cloning and reopening, out-of-order disk work, failed durable saves, journal-before-write admission, exact reconciliation evidence, draft clear/save exclusion, remote lease changes, and post-rebase recovery. Existing temporary-repository tests continue to exercise real Git and durable journals. Native GPUI tests cover cursor, folding, virtualization, and close/navigation behavior.

Generic GPUI element construction remains in `app.rs`. Moving the large render expression into the diff module exceeded the existing macro recursion limit; the shared row model, construction, metrics, and pane state are already owned by `diff_pane`. The shell also retains feature-specific presentation and imperative provider scheduling. Further extraction should remove a caller protocol or establish a useful module interface, rather than divide the file by line count.
