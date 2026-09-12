# Product requirements

Recorded on 2026-09-12. This document captures the design interview through Q59 and the later confirmation of Apple Silicon-only coverage. Q references identify the discussion that established a requirement. Later answers supersede earlier recommendations.

The user ended the interview and requested documentation. Unanswered questions are recorded in the [technical design](technical-design.md#unresolved-details); they do not block documenting the agreed scope or authorize implementation.

## Product and release scope

| Area | Agreed requirement | Origin |
| --- | --- | --- |
| Audience | Developers and teams whose members review code independently. Optimize for reviewing agent-generated PRs as well as human contributions. | Q1, Q10 |
| Workload | Usually 1–5 repositories. Optimize for ordinary application repositories; retain usable navigation for large changes. | Q4, Q59 |
| Platform | macOS on Apple Silicon only. No Intel release commitment. | Q8, final clarification |
| Providers | GitHub.com in V1. Design for additional Git providers, including GitLab. Enterprise and self-hosted installations are outside V1. | Q5, Q9, Q29 |
| Ownership | Open source, with MIT licensing for cibergit's own code. | Q7, Q30 |
| Architecture | Local desktop application using GPUI. No cibergit-operated service. | Initial request, Q23 |
| Dependencies | Require users to install Git and `gh`; use existing `gh` authentication. | Q36 |
| AI | Defer AI functionality to V2. | Q6 |
| Distribution | Runnable development previews, followed by a signed and notarized public macOS release. | Q57–Q58 |

The PR scope is everything on a PR, including review and integration. This covers the PR's lifecycle, metadata, discussions, reviewers, checks, and merge operations. Broader repository administration is outside that boundary. The exact feature inventory and treatment of unavailable APIs remain unfinished; the later proposed checklist and browser-fallback policy were not accepted.

## Workspace and sidebar

Users explicitly add local folders or remote repositories. The workspace persists that selection. Each repository has a default GitHub account, and multiple identities can be used simultaneously. GitHub holds shared collaboration state; sidebar configuration and preferences are personal. See Q10–Q12 and Q35.

The sidebar shows all open PRs by default. Every PR entry includes its title, PR number, and source branch. Filtering is a required feature.

Users compose ordered groups from repository, target branch, source branch, and PR stack. Source branch grouping supports exact names and configurable prefixes. Users save named combinations of grouping and filters. See Q13, Q18–Q19, and Q43.

Saved filters include title/number/branch search, author, reviewer, assignee, label, draft status, review status, checks, and target/source branch. Quick personal filters cover PRs needing the user's review, their own PRs, and PRs they participate in. Closed and merged PRs remain searchable. See Q12 and Q43.

## Review workspace

PRs open in tabs. Each tab preserves its selected file, scroll position, reviewed revision, and unfinished comments. A file tree drives one selected diff at a time, with next/previous-file navigation. Side-by-side is the default on wide windows; unified is the default on narrow windows. Remember an explicitly selected mode. See Q16 and Q26.

The PR header presents its title, branches, and review/merge actions. A collapsible right panel contains Overview, Activity, and Checks tabs. Review threads also appear inline in the diff. See Q27.

The visual reference is the Codex app's look and feel, combined with native macOS conventions, resizable panels, restrained colors, and light/dark themes. Keyboard and mouse receive equal emphasis. Include keyboard navigation and a command palette. See Q17 and Q42.

Every changed file appears in the file list. Do not load images or videos. Other binary files have metadata rather than text diffs. Load selected text as needed and keep large files navigable through lazy loading and virtualization. The no-media rule supersedes the earlier image-preview recommendation. See Q41, Q44, and Q59.

Supported comparisons are the full PR, an individual commit, a commit range, and changes since the user's last review. Show the active comparison above the diff. Track viewed files against a reviewed revision; changes to a file make it need another look. If the old revision is unavailable, explain why the full diff is shown. See Q37 and Q46.

## Reviews and incoming activity

Inline comments accumulate in a pending review by default, with a separate immediate-post action. Preserve unfinished text locally. Synchronize pending reviews with GitHub while online so the user can continue in the browser. Submitting a review remains explicit. See Q21 and Q47.

New remote commits produce visible feedback. They do not replace the code currently being reviewed. The user manually advances to the new revision. Comments and checks can refresh without moving the diff. See Q22 and Q34.

A review may be submitted against the older revision actually reviewed, with a clear indication that newer commits exist. Require updating to the current revision before initiating a merge. See Q48.

Merge uses a compact confirmation showing the method, commit message, branch deletion, and blockers. Remember the preferred method per repository and expose available merge, auto-merge, and queue actions according to repository settings. See Q49. Stack-specific merge behavior remains unresolved.

## Stacks

Use native stack relationships when available. Otherwise infer dependencies from PR source/target relationships. Flag ambiguity and allow personal corrections without silently changing remote PR targets. See Q32.

A combined stack diff shows the net remaining unmerged changes between its effective base and selected tip. It does not preserve the entire original feature after lower layers merge, and it does not concatenate each PR's patch. See Q13 and Q33.

The combined view is agreed. Comment routing, grouped review submission, multiple-tip behavior, and stack-wide merge controls remain unresolved. See the [open stack decisions](technical-design.md#unresolved-details).

## Local Git and source editing

Reviewing does not require a clone. When local operations are needed, create or attach a checkout. Prefer a dedicated worktree for the PR, reuse it on subsequent visits, and allow explicitly attaching an existing checkout. See Q3 and Q20.

Each PR provides two views. Review shows the selected published revision. Local Changes shows uncommitted changes and unpushed commits. Clearly indicate differences between local and remote state, including changes made outside cibergit by agents or other tools. See Q45.

Keep published diffs read-only. An Edit locally action opens the corresponding worktree file inside the same PR tab. Provide the entire worktree through a file browser and quick-open, while keeping changed files as the default review navigation. See Q55–Q56.

The focused editor includes syntax highlighting, undo/redo, find/replace, indentation, file navigation, and save. Language servers, debugging, extensions, and an integrated terminal are deferred. Use GPUI Kit's editor with compatible pinned dependencies and cibergit-specific diff/conflict interfaces. See Q31 and Q54.

External file edits automatically reload clean buffers. Preserve unsaved buffers and present a comparison when disk contents also change. Observe external commits, branch switches, and rebases. Show the actual Git state, pause incompatible operations, and preserve unsaved text while offering reconciliation or reload. See Q52–Q53.

Local actions include stage/unstage, commit, fetch, pull, push, branch switching, and branch creation. Opening a PR, including a draft, is included but is a secondary workflow. See Q14 and Q25.

Worktrees persist across sessions. Offer cleanup after PRs merge or close. Preserve uncommitted changes and unpushed commits until the user handles them. Closing a tab does not dispose of the worktree. See Q40.

## Rebase and conflicts

Interactive rebase operates on one branch at a time. Support reorder, squash, fixup, drop, reword, and edit stops for modifying or splitting commits. The V1 graphical plan handles linear histories. Histories containing merge commits require an explained external workflow rather than silent flattening. See Q15 and Q50.

Before rebasing a dirty worktree, offer commit, stash, or cancel. Restoring the stash after rebase is explicit, including any resulting conflicts. See Q51.

Provide a built-in three-way conflict view with an editable result and explicit continuation and abort controls. Also allow resolving conflicts in an external editor. See Q38.

After rebase, show the resulting commits and offer an explicit push of the rewritten branch using a lease. Do not push automatically. See Q39. The handling of affected descendant branches in a stack remains unresolved.

## Synchronization, offline work, and notifications

Poll the active PR approximately every 15 seconds and the sidebar every minute. Refresh on focus, after actions, and on manual request. Slow down when inactive or rate-limited. Local filesystem changes should appear promptly. Remote commit detection still requires the user to advance the review manually. See Q23 and Q34.

Offline operation includes cached PR reading, available local diffs, source editing, local Git operations, and review drafting. Publishing reviews, pushing, and merging require an explicit retry after failure or lost connectivity. Do not automatically replay those actions on reconnect. See Q24.

Show in-app unread indicators by default. Offer opt-in macOS notifications for review requests, mentions, replies to the user's threads, and failed checks on the user's own PRs. See Q28.

## Delivery

Deliver runnable milestones, with all agreed V1 features retained in scope. Start with repository setup, the sidebar, diffs, and synchronization. Follow with review interactions and local editing, then stacks, rebase, and release polish. See Q57.

Fast local operation is a core requirement. Numeric performance gates were proposed in Q66 but not accepted. The app update mechanism was also left unanswered.
