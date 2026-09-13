# Local rebase UI

The Rebase button in `LocalWorkspace` opens a local-only interactive rebase panel. Published Review data does not move and no remote command runs from this panel.

## Pick a base and build the plan

The panel lists local branches and the configured upstream as full commit object IDs. Manual input accepts only `HEAD`, a full 40-character commit ID, or a fully qualified `refs/*` name. Short hashes, unqualified branch names, revision expressions, and option-like input fail before inventory begins.

Prepare reads the actual checkout in the background. It records the attached branch, full base and head IDs, and a fixed oldest-to-newest commit inventory. Detached or unborn `HEAD`, an active Git operation, an unrelated base, and merge commits stop preparation with the backend's reason. The panel does not flatten merge history.

Select a row to edit it. The mouse buttons set Pick, Squash, Fixup, Drop, Reword, or Edit. Move up and Move down reorder the selected row. Command-Up and Command-Down do the same without adding global application shortcuts. Reword uses the multiline editor below the plan. Start remains unavailable until `RebasePlan::validate` accepts the exact order and actions.

Start confirmation freezes the preparation, plan, Git snapshot guard, checkout identity, and every open editor's state. Confirmation checks them again. Typing, a pending save, a changed accepted disk baseline, or a Git snapshot change pauses the request and leaves it visible. Save or reconcile editor text first, then request a new confirmation. A Git stash does not preserve text that exists only in an editor buffer.

## Dirty checkout

Dirty preparation reports staged, unstaged, untracked, and conflicted counts.

- Commit routes back to the existing staged commit controls.
- Stash including untracked asks for confirmation, then calls `RebaseStore::create_stash` with the displayed guard.
- Cancel changes no Git state.

The stash includes untracked paths. Git ignores remain outside it. After creation the panel displays the exact stash object ID and states that restore retains the stash rather than popping or deleting it.

## Stops and conflicts

The panel observes the durable lifecycle on focus, during the normal poll, and after every effect. It does not start, continue, retry, restore a stash, publish, or retire an operation on its own.

At an Edit stop, use the normal safe file editor and Local Changes staging controls. Return to Rebase to amend with the optional multiline message, begin a split, continue, or abort. Begin split displays the recorded original stop, rewritten stop, parent, required tree, and replacement count. While a split is active, generic Continue and Skip are absent. Commit staged part creates one replacement through the backend. Finish asks the backend to prove at least two replacements and the exact required tree before continuing.

Conflict rows preserve Git's raw path bytes for I/O and show the safe display form. During a rebase, ours means the already rebased series and theirs means the original commit currently being replayed. A regular UTF-8 result can open through `DocumentStore`. Save it there before Stage exact saved result. Staging re-reads the conflict stages and disk identity, then checks the current Git snapshot guard. Binary, media, non-UTF-8, missing, symlink, oversized, rename/delete, and type-change cases remain available to external Git tools. The panel does not manufacture a file or deletion for them.

Abort is a destructive confirmation. A split abort restores the recorded rewritten stop with `reset --hard` before `rebase --abort`. Git may discard recorded split or rebase edits and may delete untracked files or directories that obstruct tracked paths it restores. The UI does not claim that every untracked file survives.

## Completion, stash restore, and archive

Completed and Aborted states show the resulting commit inventory and any retained stash receipt. Restore applies that exact stash with index metadata. It never pops or deletes the stash. Restore conflicts use the same exact stage checks and require Finish stash restore after every conflict is resolved.

Close / Archive calls `retire_operation`. The backend permits it only for a conclusively inactive, safely disposable record and keeps checksummed archive evidence. The archive stops at 128 records rather than deleting old evidence. A safe archive clears the panel so another rebase can prepare.

`FailedUncertain` is read-only. There is no dismiss or archive button and no automatic retry. Restarted split state also stays a split, not an ordinary edit stop.

## Publish limit

Completion exposes the backend's rewritten-local-history handoff. `LocalWorkspaceContext` does not carry a verified pull-request source remote and branch, so this panel cannot route a publish safely. It does not guess from the local branch name. Publishing remains in the existing flow that first observes an explicit remote branch and object ID, displays that lease, and asks for a separate force-with-lease confirmation.

## Evidence limits

The native smoke example uses disposable repositories and a private temporary data root. It launches with `focus: false` and does not call `cx.activate()`. Own-scene GPUI captures prove the rendered component and its controls at the captured sizes. They do not prove physical keyboard or mouse input, Accessibility behavior, or desktop acrylic compositing.
