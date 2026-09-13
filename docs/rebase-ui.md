# Local rebase UI

The Rebase button in `LocalWorkspace` opens a local-only interactive rebase panel. Published Review data does not move and no remote command runs from this panel.

## Pick a base and build the plan

The panel lists local branches and the configured upstream as full commit object IDs. Manual input accepts only `HEAD`, a full 40-character commit ID, or a fully qualified `refs/*` name. Short hashes, unqualified branch names, revision expressions, and option-like input fail before inventory begins.

Prepare reads the actual checkout in the background. It records the attached branch, full base and head IDs, and a fixed oldest-to-newest commit inventory. Detached or unborn `HEAD`, an active Git operation, an unrelated base, and merge commits stop preparation with the backend's reason. The panel does not flatten merge history.

Select a row to edit it. The mouse buttons set Pick, Squash, Fixup, Drop, Reword, or Edit. Move up and Move down reorder the selected row. Command-Up and Command-Down do the same without adding global application shortcuts. Reword applies the current multiline editor text to that plan row. Start refuses a selected Reword row whose editor has unapplied text, and remains unavailable until `RebasePlan::validate` accepts the exact order and actions.

Start confirmation freezes the prepared base, the displayed base input, the ordered plan and every Reword message. The confirmation card prints that frozen input. Plan rows, action and reorder buttons, base controls, and the message editor stay disabled while the request is pending. Command-Up and Command-Down are consumed without moving a row. Cancel unlocks the inputs; changing the base then requires a new Prepare.

Confirm compares the current input identity with the frozen copy before it dispatches Git. A programmatic or queued plan, base, or message change pauses confirmation and keeps the same request visible. It never adopts the changed input. Cancel, edit or prepare again, then request a new confirmation. Amend and split-part confirmations use the same exact message check.

The confirmation also freezes the Git snapshot guard, checkout identity, and every open document's state. Typing, a pending save, a changed accepted disk baseline, or a Git snapshot change pauses the request and leaves it visible. Save or reconcile editor text first, then request a new confirmation. Poll and read generations are not part of the editable-input identity, so an unchanged request survives a routine observation. A Git stash does not preserve text that exists only in an editor buffer.

## Dirty checkout

Dirty preparation reports staged, unstaged, untracked, and conflicted counts.

- Commit routes back to the existing staged commit controls.
- Stash including untracked asks for confirmation, then calls `RebaseStore::create_stash` with the displayed guard.
- Cancel changes no Git state.

The stash includes untracked paths. Git ignores remain outside it. After creation the panel displays the exact stash object ID and states that restore retains the stash rather than popping or deleting it.

## Stops and conflicts

The panel observes the durable lifecycle on focus, during the normal poll, and after every effect. It does not start, continue, retry, restore a stash, publish, or retire an operation on its own.

Routine lifecycle views lead with a human state label, branch, short base ID, and the stopped commit headline when available. Show operation details reveals the full operation, base, original, onto, stopped, todo, done, and ownership identities. Conflict disk digests and device/inode identity also appear only in that expanded view. Hiding these diagnostics does not change the guard or command payload.

At an Edit stop, use the normal safe file editor and Local Changes staging controls. Return to Rebase to amend with the optional multiline message, begin a split, continue, or abort. A pending Amend freezes the raw editor text, including the empty-text choice that keeps the existing message. Begin split displays the recorded original stop, rewritten stop, parent, required tree, and replacement count. While a split is active, generic Continue and Skip are absent. Commit staged part freezes its displayed message before confirmation and creates one replacement through the backend. Finish asks the backend to prove at least two replacements and the exact required tree before continuing.

Conflict rows preserve Git's raw path bytes for I/O and show the safe display form. Open sources and result displays Git's three source stages without reading them from the worktree. During a rebase, the labels are Base, Already rebased (ours), and Replayed commit (theirs). Stash restoration uses Current worktree (ours) and Restored stash (theirs). Each source shows its short object ID and mode. Show full source details reveals the full object ID. A missing stage says that Git deleted the path; it never appears as an empty file.

The source layout follows the current pane width. A wide pane shows all three sources side by side. A narrow pane shows one source at a time and keeps the editable result below it. Use the source buttons or Command-Left and Command-Right to change the visible source. Each source scrolls horizontally, so a long line remains available through its final byte instead of wrapping into an unreadable column.

The result is the same `DocumentStore` document used by Local Changes. Opening a source view, changing the selected source, refreshing source stages, and closing the presentation do not save the file or replace editor text. Editor changes keep the normal undo, recovery, persist, save, reload, and reconcile history. Save result writes through `DocumentStore`; it does not stage. Stage saved result remains a separate confirmed command.

The presentation freezes the operation ID, active rebase identity or stash-restore identity, raw path, all three stage objects, and the observed disk generation. If Git replaces a stage, resolves the path, or ends the operation, the old sources stay visible as frozen evidence and staging stops. Refresh changed stages accepts a replacement only for the same operation identity and raw path. It never changes the result buffer. A new operation cannot claim an old presentation just because it conflicts on the same path.

An external disk change is a different conflict from Git's unmerged stages. If `DocumentStore` reports a disk/base/buffer conflict, the result editor and staging controls stop. Open disk reconciliation in Local Changes to reload or reconcile the exact disk version. The Git sources remain read-only while that workflow runs.

A regular UTF-8 result supports the full internal edit, undo, save, stage, and continue path. Binary, media, non-UTF-8, missing result, symlink, oversized, rename/delete, and type-change cases show source metadata but still require an external Git tool. The panel does not launch another application. It also does not create, remove, rename, or stage those paths on its own.

Staging re-reads the conflict stages and disk identity, then checks the current Git snapshot guard and every open document. Immediate typing, a pending persist, an unsaved document, or changed source objects stops the confirmation. After staging, the panel reads Git again and marks the presentation resolved only when the path has left the unmerged index.

Abort is a destructive confirmation. A split abort restores the recorded rewritten stop with `reset --hard` before `rebase --abort`. Git may discard recorded split or rebase edits and may delete untracked files or directories that obstruct tracked paths it restores. The UI does not claim that every untracked file survives.

## Completion, stash restore, and archive

Completed and Aborted states show the resulting commit inventory and any retained stash receipt. Restore applies that exact stash with index metadata. It never pops or deletes the stash. Restore conflicts use the same exact stage checks and require Finish stash restore after every conflict is resolved.

Close / Archive calls `retire_operation`. The backend permits it only for a conclusively inactive, safely disposable record and keeps checksummed archive evidence. The archive stops at 128 records rather than deleting old evidence. A safe archive clears the panel so another rebase can prepare.

`FailedUncertain` is read-only. There is no dismiss or archive button and no automatic retry. Restarted split state also stays a split, not an ordinary edit stop.

## Publish limit

Completion exposes the backend's rewritten-local-history handoff. `LocalWorkspaceContext` does not carry a verified pull-request source remote and branch, so this panel cannot route a publish safely. It does not guess from the local branch name. Publishing remains in the existing flow that first observes an explicit remote branch and object ID, displays that lease, and asks for a separate force-with-lease confirmation.

## Evidence limits

The native smoke example uses disposable repositories and a private temporary data root. It launches with `focus: false` and does not call `cx.activate()`. Own-scene GPUI captures prove the rendered component and its controls at the captured sizes. They do not prove physical keyboard or mouse input, Accessibility behavior, or desktop acrylic compositing.
