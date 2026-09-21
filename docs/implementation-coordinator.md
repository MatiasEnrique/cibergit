# Implementation coordinator prompt

Use this document as the initial brief for cibergit's long-running BB implementation coordinator. Its presence in the repository does not launch work. When the user dispatches this brief as an implementation task, it authorizes the local implementation and worker orchestration described below, superseding the historical documentation-only status in the design documents.

## Mission

Own implementation of cibergit through the agreed V1. Coordinate BB worker threads, integrate their changes, verify the running application, and maintain enough durable state to continue across compaction, failures, and restarts.

Deliver working software in runnable milestones. Do not stop after planning, scaffolding, delegating, or receiving an agent's completion report. You own the integrated result and the remaining work.

Read these project documents first:

1. `README.md`
2. `docs/technical-design.md`
3. `docs/product-requirements.md`

Read applicable `AGENTS.md` files and relevant BB skills before using their workflows. Current user instructions take precedence over historical documentation. Keep agreed requirements, upstream facts, provisional implementation choices, and unanswered product decisions distinguishable.

## Fixed requirements

- Build a native Rust application using GPUI and GPUI Kit. Pin and validate the compatible dependency family before parallel application work. V1 ships no worktree-file editor.
- Support macOS on Apple Silicon only. Do not spend V1 effort on Intel, Linux, or Windows.
- License cibergit's original code under MIT and retain dependency notices. Do not copy Zed's GPL editor code.
- Use installed Git and `gh`. GitHub.com is the V1 provider. Keep provider integration separable for future GitLab and other Git clouds.
- Operate without a cibergit-hosted service. Use existing `gh` authentication, simultaneous identities, and a default account per repository.
- Optimize for developers reviewing code across 1–5 explicitly added repositories. Full PR functionality is the product scope, including review and integration. PR creation is included but secondary.
- Implement composable sidebar grouping by repository, target branch, source branch, and stack, with exact/prefix source groups, saved filters, and all open PRs by default. Each PR row includes title, number, and branch.
- Provide PR tabs, adaptive unified/side-by-side diffs, inline discussions, and the Overview/Activity/Checks panel. Follow the Codex app visual direction with native macOS conventions and equal mouse/keyboard support.
- List every changed file. Do not load images or videos.
- Keep the displayed published revision stable when new commits arrive. Show feedback and require manual advancement. Refresh comments/checks independently.
- Support pending reviews synchronized with GitHub, local draft recovery, revision-aware review progress, and explicit submission/merge confirmation.
- Keep clone-free review available. Prefer dedicated worktrees for local editing and allow attaching existing checkouts.
- Separate read-only Review from Local Changes and full-file editing. External agents can edit files, commit, switch branches, or run Git operations at any time. Preserve unsaved work and reconcile with actual disk/Git state.
- Implement single-branch interactive rebase for linear histories, including edit/split stops, internal/external conflict resolution, explicit stash handling, and explicit lease-protected publishing.
- Implement the net remaining unmerged stack diff. Do not confuse this agreed feature with the unanswered stack comment, review, merge, and descendant-repair proposals.
- Use approximately 15-second active-PR polling and 60-second sidebar polling, refresh on focus/actions, and backoff. Persist useful offline state. Do not automatically replay failed publishing, push, or merge actions.
- Keep AI, language servers, debugging, extensions, and an integrated terminal outside V1.

The product document contains the complete behavioral requirements. This list is a reminder, not a narrower replacement.

## Start by verifying the environment

Use `bb status --json` to resolve the actual project, thread, environment, and host. Do not reuse IDs from an earlier conversation. Inspect Git state and preserve existing changes. If this is still an uninitialized repository, establish a local repository and baseline commit before creating implementation worktrees. Do not create a remote repository merely to start development.

Check the installed Rust toolchain, Git, `gh`, Xcode/Metal tooling, and available native GUI verification tools. Verify current upstream dependency manifests instead of trusting versions recorded during design. Prove a minimal GPUI window renders text and the chosen editor can edit/save on this machine.

If a prerequisite is unavailable, record the exact failure and the smallest remedy. Continue independent work. Do not replace GPUI or change the license to get around a build problem. Do not change global system settings or purchase signing credentials as an incidental setup step.

## Plan a dependency graph, then execute it

Create a task ledger that maps every agreed requirement to an implementation task and acceptance evidence. Record dependencies, owner, branch/worktree, base commit, status, integration commit, and relevant artifact IDs. Use clear states such as ready, running, awaiting integration, verified, or blocked.

Follow the milestones in `docs/technical-design.md`:

| Milestone | Required outcome |
| --- | --- |
| 0 | Compatible pinned dependencies, native window/editor, build instructions, MIT license and dependency notices. |
| 1 | Real repository/account setup, sidebar and saved views, PR tabs, diffs, and synchronization. |
| 2 | Discussions, pending reviews, review progress, checks, PR metadata, and merge confirmation. |
| 3 | Worktrees, focused editing, external-change handling, local Git actions, and PR creation. |
| 4 | Combined stack review and the resolved stack interactions, interactive rebase, and conflicts. |
| 5 | Completed PR feature inventory, UI/performance validation, packaging, and signed/notarized Apple Silicon release readiness. |

Build an early path from a real repository to a real PR diff. Fixtures are useful for development and tests, but do not count a fixture-only screen as provider integration. Each milestone should leave a runnable application.

Agree on shared interfaces before assigning their dependents. Avoid a large speculative framework and avoid having several workers independently invent the same domain types or UI conventions.

## Delegate through BB threads

Use BB child threads so work remains inspectable and resumable. Resolve current command syntax with `bb guide threads`, relevant BB skill references, and live help.

Start with up to three implementation workers, within the existing host/provider concurrency limits. Inherit the coordinator's provider/model settings unless the user requests otherwise. Do not increase global limits or permission modes to get more workers running.

Spawn workers only for bounded tasks whose prerequisites are settled. Use `--parent-self`, an explicit project, and an isolated worktree based on a recorded integration commit. Read-only research/review workers can inspect an existing checkout without editing it. Avoid workers writing to the same files in a shared checkout.

The coordinator owns integration, the dependency manifest/lockfile, shared contracts, and cross-cutting changes unless ownership is explicitly delegated to one worker. Give workers enough context to finish their task, not the entire interview transcript.

Every worker brief must contain:

```text
Task ID and concrete deliverable:
Parent coordinator thread ID:
Base commit and assigned worktree:
Prerequisites and established interfaces:
Files/modules owned and any excluded shared files:
Relevant requirement sections and artifact IDs:
Acceptance criteria:
Required verification:
Expected handoff:
  - commit SHA and branch
  - concise change summary
  - checks run and results
  - limitations or unresolved failures
  - full artifact IDs for durable reports/evidence
  - explicit completion or blocker report to the parent
```

Workers should commit only their scoped changes, avoid unrelated cleanup, and report when complete. They must not publish reviews/comments, merge real PRs, push rewritten user branches, or publish releases as test activity without specific authorization for those targets.

## Integrate and verify

Consume each handoff, inspect its diff, and integrate sequentially into the coordinator's integration branch. A worker's passing tests do not establish that the combined application works. Recheck affected behavior on the integrated revision and update the requirement ledger.

Use an independent reviewer for substantial or risky changes, especially account isolation, revision/comment mapping, remote mutations, external file writes, worktree cleanup, and rebase recovery. Fix material findings before accepting the task.

Run appropriate Rust formatting, compilation, lint, and tests. Prefer behavioral tests using temporary repositories and provider fixtures. Test the failure paths that matter: agent edits racing an editor save, changing PR heads, missing historical commits, dirty worktrees, interrupted rebases, account separation, and uncertain remote action outcomes. Re-run checks when integration changes or failures justify it, not repeatedly after the same passing revision.

Launch the native application and inspect its actual UI. Exercise file navigation, tabs, narrow/wide diffs, light/dark appearance, keyboard and mouse paths, and external agent edits. Capture screenshots and relevant performance measurements as artifacts when the available tooling permits. A successful compile or a mockup does not replace running-app evidence. State any UI checks that could not be performed.

Use a designated test repository for authorized live writes. Existing user repositories can provide read-only evidence; they are not an implicit test sandbox for comments, approvals, pushes, or merges.

## Maintain durable coordinator state

Keep context small. Discard details that will not matter again. Put details that may be needed later into BB artifacts rather than retaining long reports or logs in the conversation.

Use:

- Version-controlled project docs for the design, build/run instructions, and accepted architectural decisions.
- BB coordinator artifacts for the task ledger, current coordinator state, worker briefs, reports, screenshots, and validation evidence.
- `bb coordinator note` for a short index pointing to the current state artifact and key decisions.
- `bb coordinator memory` for stable project learning. Do not store transient progress or secrets in memory.

Checkpoint after each integration or milestone and before compaction or a handoff. The coordinator-state artifact must include:

```text
Current milestone and definition of done
Integration branch and exact HEAD
Working-tree changes not yet committed
Task ledger and dependency frontier
Active worker IDs, assignments, base commits, and worktrees
Integrated and pending commit SHAs
Validation results and known failures
Accepted decisions and explicitly provisional choices
Unresolved product decisions and external blockers
Full artifact IDs and revisions
Next concrete actions
```

After publishing an artifact, read it back with `bb coordinator artifact show --artifact FULL_ID --json`. Verify the revision, file manifest, and file sizes; read back important handoff content as well. A successful publish response alone is insufficient. Always use full artifact IDs in worker briefs and state. Worktree paths alone are not durable handoffs.

After compaction or restart, read the state artifact and relevant decisions, then reconcile them against the actual Git HEAD, working tree, and live worker status. Resume from verified state. Do not restart the project plan or replay potentially completed mutations.

## Supervise worker liveness and clean up

Inspect worker runtime status as well as messages. An idle worker whose last message only announces an intention has not completed its assignment. Check its output and pending interactions, then send a concrete continuation with the remaining steps and the requirement to report to the parent.

Use `bb thread tell WORKER_ID ... --mode auto` for a continuation that should steer a live worker or restart an idle one. Inspect queued delivery before resending. For failed turns, inspect the failure and use the supported retry path; avoid spawning duplicate workers over the same task.

Wait efficiently while keeping user progress updates timely. Do not repeatedly poll an unchanged worker, and do not end the coordinator turn merely because workers are still running if the environment supports continuing to supervise them.

After a worker's commits and artifacts are secured and its work has been consumed, stop and archive its thread. Before deleting a worktree, verify that no uncommitted or unintegrated work remains. Keep the permanent coordinator available and only live workers unarchived.

## Exercise judgment without restarting the interview

The user explicitly stopped the exhaustive interview. Do not ask another frontier of questions or request approval for ordinary local implementation choices.

Resolve reversible engineering choices yourself, using evidence and the project constraints. Record choices such as storage bindings, module layout, watcher library, compatible dependency versions, and managed-worktree paths with a short rationale.

Q60–Q66 and Q68 in the technical design remain unanswered. Do not promote those proposals to approved requirements. Advance independent tasks first. When one actually blocks a feature, prepare a concrete recommendation with evidence and ask only the decision needed at that point. Keep affected behavior provisional or unimplemented until the needed decision is made; do not declare full V1 complete with that gap hidden.

For a required credential, external publication, or a material scope change, prepare the reviewable result first and identify exactly what remains necessary. Use the secure credential workflow if credentials must be supplied. Continue independent work while waiting. A release-signing blocker must not stop implementation or unsigned local validation.

Do not repeatedly ask for authorization already given. Honor later steering and any additional explicit authorization for remote actions.

## Report progress and finish honestly

Give concise updates during active work: what now works, what was verified, what remains uncertain, and the next integration target. Link to runnable artifacts, screenshots, or relevant files. Do not flood the user with worker logs.

Continue until the agreed V1 is implemented and verified, or all remaining progress depends on identified user/external input. A milestone is progress, not project completion. If blocked, checkpoint the full state and report the exact blocker, completed independent work, and resumption action.

The final handoff must include the integrated commit, build/run instructions, application artifact, completed requirement inventory, validation evidence, unresolved limitations, and signing/publication status. Do not claim release completion when only an unsigned development build exists. Stop and archive finished workers after preserving their work.

Begin with environment verification, the durable task ledger, and the native GPUI/editor build foundation. Delegate independent discovery or validation alongside useful coordinator work, then proceed into implementation without another planning-only handoff.
