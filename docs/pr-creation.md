# Native pull request creation

The native review workspace can create a normal or draft GitHub pull request from an explicitly added repository. Open **Create pull request** in the sidebar or press `⇧⌘N`.

The form requires all remote coordinates explicitly:

- the selected GitHub account and target repository;
- the published target base branch;
- a same-repository or verified-fork source repository already added for that account;
- the published source branch;
- title, Markdown body, and normal/draft state.

A local branch name is optional and informational. It is displayed and saved, but it never chooses a provider branch and is never pushed automatically. Branch suggestions are bounded provider reads; truncated or incomplete choices are labeled. A typed branch is still verified by GitHub during preparation.

## Prepare, confirm, create

**Prepare creation** performs a read-only provider preparation in the background. It verifies the selected credential, canonical repositories and fork network, published base and source refs, their full OIDs, and creation capability. The resulting confirmation freezes every visible field, the selected account, both repository/branch pairs, both observed OIDs, capability, and normal/draft state.

GitHub's create endpoint cannot atomically require the reviewed source OID. The confirmation says so plainly. Confirming performs the accepted sequence only:

1. repeat the provider preflight;
2. acquire the private source-to-target creation authority;
3. durably record the exact request and provider context as `InFlight`;
4. repeat preflight while authority remains held;
5. dispatch exactly one creation request;
6. save `NotStarted`, `Acknowledged`, or `Uncertain` before releasing authority.

The form's controls are inert while its final save, preparation, or creation is pending. **Prepare creation** first drains the serialized draft-save lane and starts the provider read only when the exact visible form is durable. The full confirmation disclosure includes the exact Markdown body, not merely its length. A keyboard edit that nevertheless changes an input invalidates the pending witness. Confirm independently compares the current visible form with the frozen confirmation and sends zero writes on mismatch.

An acknowledgement shows the actual GitHub head separately from the reviewed preparation head. If they differ, the actual head is never described as reviewed. **Open PR** uses the exact acknowledged repository and PR number and leaves unrelated review tabs intact. Opening a historical recovery row first saves the latest editable form, then opens that row's PR even if another acknowledgement is displayed. A failed save keeps the form open, and navigation is disabled while creation is pending.

## Durable state and recovery

Creation state lives under the app data directory in `pr-creation/`. Directories are mode `0700`; files are mode `0600`. Draft and journal records are versioned and bounded. Writes use a private temporary file, file sync, atomic rename, and directory sync. Draft saves use a generation compare-and-swap and one coalescing lane: a successful older save advances the owned durable baseline, then the latest pending text saves against it. A late window cannot overwrite newer durable text. Closing queues the latest visible form immediately, without waiting for the typing debounce, and the dialog remains open if that save fails.

Every authority lane is the exact tuple:

`github + selected account + target repository + base branch + source repository + published source branch`

GitHub login, owner, and repository identity use their accepted ASCII case-insensitive form. Branch names remain exact and case-sensitive. Title, body, draft state, local branch, capabilities, and observed OIDs are stored in the exact attempt context but do not split the authority lane. Thus two forms that differ only in text or OIDs still serialize against one another.

Authority lock files are opened without following symlinks and must be single-link regular files. The inode is checked before and after locking. Admission uses a bounded nonblocking lock wait and refuses with zero writes instead of stalling the UI. Unlock is explicit, including error exits and duplicated-descriptor cases. `InFlight` is synced before provider dispatch, and losing a terminal save leaves the durable `InFlight` record in place.

Corrupt, future-version, oversized, symlinked, or multiply linked originals are preserved and refused. Recovery records from every stored tuple remain visible even when the editable form switches repositories or branches.

`InFlight` and `Uncertain` are no-replay states. An unknown PR ID, lost provider reply, or terminal-save failure is not resolved by body search, branch-only guessing, or treating absence as proof that creation did not occur. The UI retains the exact request and explains that explicit external reconciliation is required. There is no automatic retry. A durable acknowledgement can be reopened after restart using its exact returned coordinates.

A conclusive `NotStarted` record proves that provider dispatch did not begin. It retains its exact prior context but permits a new explicit retry only with fresh operation and attempt identities. Reusing a terminal identity is refused, and an acknowledged exact frozen creation remains duplicate-blocked.

## Verification boundary

Automated creation transport uses production-shaped fake `gh` payloads only. Native smoke evidence may use real read-only repository and branch preparation against an explicitly chosen public fixture, followed by synthetic acknowledgement controls to demonstrate moved-head presentation. The smoke never presses **Create pull request** and invokes zero live creation transports. This feature does not authorize live writes, authentication changes, settings changes, pushes, or local branch mapping.
