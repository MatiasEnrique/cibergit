# Technical design

Recorded on 2026-09-12. Read this with the [product requirements](product-requirements.md).

This document distinguishes agreed choices from implementation proposals and upstream facts. Module names, storage engines, and internal data structures below are design recommendations, not code that has been built or additional user decisions. The current task authorizes documentation only.

## Technology decisions

| Concern | Decision | Implementation consequence |
| --- | --- | --- |
| Language and UI | Rust with GPUI | Native desktop rendering and GPUI-managed application state. |
| Target | macOS, Apple Silicon only | Target `aarch64-apple-darwin`; do not require an Intel or universal build. |
| Editor | GPUI Kit's focused editor | Pin a compatible GPUI family. Implement review diffs, line comments, and conflict presentation in cibergit. |
| License | MIT for cibergit | Preserve dependency licenses and notices. Do not copy Zed's GPL editor implementation into this design. |
| Remote integration | GitHub.com through `gh` in V1 | Use structured CLI output and authenticated REST/GraphQL through `gh api`. |
| Future providers | GitLab and other Git clouds | Isolate provider transport, identifiers, capabilities, and review semantics from views. |
| Local access | Fast local repository and worktree access | Prefer local Git objects when available; retain a remote review path without a clone. |
| Authentication | Existing `gh` logins, account per repository | Every request and stored private resource needs an explicit account context. |
| Synchronization | Polling, no hosted service | Separate remote refresh from displayed revision changes. |
| Distribution | Signed/notarized public macOS app | Development previews can precede release signing. Update delivery is unresolved. |

## Dependency and build constraints

GPUI is pre-1.0 and changes frequently. Its current macOS instructions require Xcode and Metal tooling. Follow the instructions for the pinned revision and verify a minimal native window on Apple Silicon before building application features. Select the minimum supported macOS version during that build check. [GPUI documentation](https://raw.githubusercontent.com/zed-industries/zed/main/crates/gpui/README.md)

GPUI Kit provides a reusable source editor and an Apache-2.0 base crate. Its editor can supply syntax highlighting, search, indentation, and read-only text presentation, with application-controlled styling. Full PR diff alignment and a three-way conflict view still need implementation. [Editor documentation](https://gpui-kit.com/base/primitives/editor/), [base crate manifest](https://raw.githubusercontent.com/longbridge/gpui-kit/main/crates/base/Cargo.toml)

At documentation time, GPUI Kit's workspace uses version 0.6.1 and aliases its GPUI dependency to `gpui-pre` 0.3.1, with matching platform and macro packages. These are verified upstream versions, not a tested cibergit lockfile. Resolve and pin the compatible family together; do not independently mix an arbitrary `gpui` release with the editor. [Workspace manifest](https://raw.githubusercontent.com/longbridge/gpui-kit/main/Cargo.toml)

Zed's editor crate declares GPL-3.0-or-later and depends on internal application crates. The selected alternative is GPUI Kit under its own dependency terms, with MIT for cibergit's original code. [Zed editor manifest](https://raw.githubusercontent.com/zed-industries/zed/main/crates/editor/Cargo.toml)

Rust, Git, `gh`, and Apple developer tooling should be checked before implementation. Git and `gh` are runtime prerequisites for end users; Xcode is a build prerequisite. No dependency installation or application build has been performed as part of this documentation task.

## Design tree

```text
cibergit
├── Local desktop review application
│   ├── macOS / Apple Silicon
│   ├── Rust / GPUI / GPUI Kit editor
│   ├── MIT original code / retained dependency notices
│   └── Codex-app visual direction / equal keyboard and mouse support
├── Independent developer workspace
│   ├── Explicit repositories / default account per repository
│   ├── Composable groups / saved filters / all open PRs by default
│   └── PR tabs / persisted view state
├── Remote collaboration
│   ├── GitHub.com / gh and gh api
│   ├── Future provider adapters, including GitLab
│   ├── Polling / no cibergit service
│   └── Pending reviews / checks / discussions / full PR functionality
│       └── Open: detailed parity inventory and API-gap policy
├── Revision-aware review
│   ├── Full PR / commit / range / changes since last review
│   ├── Published revision stays fixed until manual update
│   └── Combined stack diff / net remaining unmerged changes
│       └── Open: ambiguous graphs, comments, grouped reviews and merges
├── Local work
│   ├── Review without clone / worktree when needed
│   ├── Read-only Review / editable Local Changes
│   ├── Full worktree file navigation / focused editor
│   ├── Observe external agent and Git changes / preserve dirty buffers
│   └── Single-branch graphical rebase / linear history / explicit push
│       └── Open: stack descendant repair workflow
└── Delivery
    ├── Runnable milestones / signed and notarized public V1
    └── Open: numeric performance gates and application updates
```

## Proposed application boundaries

Start with these logical responsibilities. They can be modules in one Cargo package; this document does not require a multi-crate workspace.

| Module | Responsibility |
| --- | --- |
| `app` | GPUI lifecycle, windows, actions, settings, and appearance. |
| `workspace` | Repository selection, accounts, sidebar views, and PR tab state. |
| `domain` | Provider-aware identities, immutable review revisions, comments, checks, stack relationships, and operation states. |
| `providers` | Capability-oriented interface with a GitHub adapter backed by `gh`. |
| `git` | Local status, objects, diffs, refs, worktrees, commits, and rebase execution. |
| `review` | Diff comparisons, inline coordinates, pending reviews, and viewed-file progress. |
| `editor` | GPUI Kit integration, disk/buffer reconciliation, and conflict editing. |
| `sync` | Poll scheduling, filesystem events, invalidation, backoff, and reconnect behavior. |
| `storage` | Account-partitioned cache, drafts, preferences, progress, and session restoration. |

Keep CLI calls and Git parsing outside GPUI views. GitHub response types stay inside the provider adapter. Shared models need provider, host, and opaque remote IDs so a future GitLab adapter does not have to pretend an MR is a GitHub resource.

Model provider capabilities explicitly. A provider may support different approval rules, pending reviews, check actions, or merge modes. Exact trait signatures and error types remain implementation details.

## GitHub transport and accounts

Use high-level `gh` commands when they expose the required structured fields. Use `gh api` for authenticated REST and GraphQL operations that do not have a complete high-level command. The CLI dependency does not limit the product to `gh pr` subcommands. [GitHub CLI API manual](https://cli.github.com/manual/gh_api)

Proposed transport behavior:

- Invoke executables with argument arrays and structured stdin; do not construct shell commands from branch names or comment text.
- Set repository, host, and account context explicitly rather than relying on the process working directory or active CLI login.
- Paginate lists and distinguish empty, loading, stale, unavailable, and failed states.
- Run subprocesses away from the UI thread, with cancellation and bounded output.
- Partition cached data and unfinished reviews by account as well as repository and PR.

`gh auth token --hostname HOST --user USER` can retrieve a specific stored account's token. `GH_TOKEN` selects the credential for a child process. A proposed implementation can use these together without changing the global active account. Keep credentials in private memory and child environment only, never logs, command arguments, or the content cache. This mechanism still needs integration testing. [Account token selection](https://cli.github.com/manual/gh_auth_token), [CLI environment](https://cli.github.com/manual/gh_help_environment)

Local Git transport credentials and commit authorship are separate from the GitHub API identity. Implementation must verify how an existing checkout authenticates fetch/push and show failures without silently changing global Git configuration. A per-repository GitHub account does not by itself change Git's commit identity.

## State ownership and revision model

Keep the following sources of truth distinct:

| State | Authority | Local representation |
| --- | --- | --- |
| PR title, metadata, discussions, reviews, checks | GitHub | Cached snapshot with last refresh and account context. |
| Published code being reviewed | Selected immutable commit comparison | Base/head object IDs and comparison mode. |
| Current local branch, index, saved files, rebase state | Git repository and worktree | Observed state that can change outside cibergit. |
| Unsaved editor text | Editor buffer | Buffer version plus the disk version it was based on. |
| In-progress comment text | Local draft until synchronized/submitted | Durable text, remote draft IDs, and synchronization status. |
| Sidebar views, tabs, review progress | Personal local state | Persistent configuration and revision-scoped progress. |

Recommended identities include provider, host, repository ID, PR ID/number, and account ID. A review comparison also needs immutable base/head SHAs and its mode. A local worktree record needs its path, Git directory, common Git directory, current HEAD, and association with the PR.

Do not key a diff cache or review draft by branch name alone. Branches move, PR numbers repeat across repositories, and multiple accounts can have different access to the same host.

The diff pipeline should consume an immutable comparison. Prefer local objects and Git diffs when available. Use GitHub data for clone-free review; identify incomplete or unavailable comparisons rather than presenting a truncated patch as complete. Switching data sources must preserve the selected published revision and comment coordinates.

## Synchronization and persistence

Agreed remote polling defaults are approximately 15 seconds for the active PR and 60 seconds for the sidebar, with refresh on focus, manual refresh, and refresh after actions. Back off while inactive or rate-limited. New head SHAs update an availability indicator, not the active comparison.

Use conditional HTTP requests where supported and avoid concurrent refresh storms. GitHub documents conditional requests and rate-limit-aware retry behavior. Actual retry timing should follow returned headers. [REST API best practices](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api)

Proposed local persistence is SQLite for queryable cached data, drafts, and operation records, with versioned configuration for personal settings. The storage engine, Rust binding, and configuration format were not selected during the interview. Persist durable draft text separately from disposable caches.

Read refreshes may resume automatically after reconnect. Publishing reviews, pushing, and merging require explicit retry. Preserve an uncertain remote mutation outcome and check whether it succeeded before retrying; a timeout does not prove the server rejected it. Online synchronization of pending private reviews must not become implicit submission of offline work.

The local watcher should observe worktree files and the actual Git directories, including refs, index, and operation state. Worktree metadata may live outside the checkout. Debounce event bursts and re-read authoritative state after events and app focus. Filesystem events are invalidation hints, not a transaction log.

For external edits, compare the disk version against the buffer's base version. Reload clean buffers. Preserve dirty buffers and show reconciliation when both sides changed. Before saving, check disk state again so a delayed watcher does not overwrite an agent's latest edit.

## Local Git and worktrees

The proposed initial Git backend uses the installed `git` executable with structured output, including NUL-delimited paths where available. This avoids requiring a second Git implementation before core workflows work. A library backend can be evaluated later if measurement justifies it.

Dedicated worktrees are the default for editing a PR, and an existing checkout can be attached explicitly. Keep clone-free reading independent from creating a managed local repository. The location and layout of managed clones/worktrees remain implementation details.

Model Review and Local Changes separately. Review follows immutable published SHAs. Local Changes follows the real index, working tree, local commits, and remote tracking state. External agents can change all of these without using cibergit.

Serialize cibergit's incompatible writes to a worktree and account for refs shared across worktrees. External tools do not participate in an application lock, so revalidate Git state immediately before operations and handle Git's own lock errors. Never infer exclusive ownership from having created the worktree.

Persist worktree associations. Offer cleanup of closed/merged PR worktrees after checking uncommitted files and unpushed commits. Tab closure is unrelated to cleanup.

## Rebase execution

Translate the graphical plan into Git's interactive rebase mechanism, with an application-controlled sequence editor and explicit operation state. Git supports rebase continuation, skip, and abort; the GUI should reflect the repository's actual operation state rather than only the last button pressed. [Git rebase documentation](https://git-scm.com/docs/git-rebase)

The plan supports reorder, squash, fixup, drop, reword, and edit stops. Edit stops expose local editing and staging needed to modify or split a commit. Detect merge commits before starting the V1 graphical plan and direct those histories to the external workflow.

The operation lifecycle needs preparation, running, paused-for-edit, conflicted, completed, aborted, and failed states. Reinspect Git state on restart and when an external tool continues or aborts the operation.

Require the user to handle dirty changes by committing, stashing, or cancelling. Track a created stash explicitly so restoration addresses the correct entry. Restore only on request. Provide an editable three-way conflict result and an external-editor option.

After successful rebase, publishing is a separate user action. Use an explicit expected remote head for the force-with-lease check, rather than assuming an automatically refreshed tracking ref still represents the user's observed remote state. Stack descendant repair is not yet specified.

## Stack representation and upstream constraints

Agreed behavior uses authoritative native stack relationships where available, then inferred PR branch dependencies with visible ambiguity and personal corrections. Record whether a relationship is native, inferred, or a personal correction.

A combined diff represents net remaining unmerged changes. Resolve an effective base and selected tip; do not concatenate intermediate patches. Multiple tips do not define a single final tree without choosing a path or synthesizing a merge, and that product choice remains open.

GitHub's native stacks are currently in public preview. REST supports stack management, while GraphQL exposes read-only membership. Native stack merges require the asynchronous merge API and include every lower PR through the selected layer. A successful initial request is not proof of completion; rules can fail during background execution. These are provider constraints, not acceptance of Q63's proposed stack-merge UI. [Stack API documentation](https://docs.github.com/en/pull-requests/reference/stacked-pull-requests-apis-and-webhooks)

GitHub inline comments require a destination PR and commit/path/line coordinates. Preserve provider coordinates separately from displayed aggregate-diff coordinates. Mapping combined-diff comments to underlying reviews, especially where layers edit the same lines, needs a defined product rule. [Review comment API](https://docs.github.com/en/rest/pulls/comments)

## Implementation milestones

The order below implements the accepted runnable-milestone approach. It does not narrow the agreed V1 scope.

| Milestone | Deliverable | Evidence needed |
| --- | --- | --- |
| 0. Build foundation | Pinned GPUI/editor dependencies, native Apple Silicon window, license inventory. | Build, render text, edit/save a file, verify dependency notices. |
| 1. Read and synchronize | Add repositories/accounts, all-open sidebar, filters/groups, PR tabs, local/remote diffs, polling. | Review both cloned and uncloned PRs; new remote commits leave the selected revision stable. |
| 2. Participate in reviews | Discussions, pending drafts, review submission, progress, checks, PR metadata, merge confirmation. | Revision-correct comments, restart/offline recovery, account separation, and explicit publication. |
| 3. Work locally | Worktrees, focused editor, full-file navigation, local Git actions, secondary PR creation flow. | External agent edits and branch changes remain visible without losing unsaved text. |
| 4. Stacks and rebase | Combined stack diffs, agreed stack interactions once resolved, graphical rebase and conflicts. | Linear-history editing, conflict continuation/abort, explicit push, correct aggregate comparisons. |
| 5. Public V1 | Complete PR capability inventory, UI polish, performance validation, signed/notarized Apple Silicon app. | Functional review of agreed scope, install/launch verification, representative local performance results. |

Validation should prioritize behavior that can corrupt a review or lose local work: external edits during a save, revisions changing during submission, partial/uncertain remote outcomes, dirty worktrees, and rebase recovery. Use temporary repositories for Git scenarios and recorded or synthetic provider responses for routine tests. Live provider validation belongs in explicitly designated test repositories.

## Unresolved details

The user stopped the interview before answering Q60–Q66 and Q68. These remain proposals, not defaults to silently implement. Q67 was resolved separately as Apple Silicon only.

| Topic | Last proposal, not accepted | Dependency |
| --- | --- | --- |
| Q60: multiple-tip stack diff | Select one tip and show its dependency path. | Branched stack UI and comparison selection. |
| Q61: aggregate inline comments | Post directly only with unambiguous PR/line mapping; otherwise open an individual PR. | Combined-diff commenting. |
| Q62: grouped stack reviews | One panel submits distinct per-PR decisions and reports individual results. | Multi-PR submission behavior. |
| Q63: stack merge | Native asynchronous merge with affected PRs shown; sequential guidance for inferred stacks. | Stack merge controls. |
| Q64: rebase descendants | Show affected descendants and guide individual updates without automatic cascading. | Repair workflow after rewriting a stack branch. |
| Q65: exact PR parity | Formal feature checklist, with browser fallback only for verified API gaps. | Final parity acceptance. Broad full-PR scope is already agreed. |
| Q66: performance gates | Cached PR switch within 100 ms and first useful local diff within 500 ms. | Numeric benchmarks. Fast local interaction is already required. |
| Q68: app updates | Check GitHub Releases and offer explicit installation. | Update mechanism. Signing/notarization is already agreed. |

Engineering details still need validation during implementation: compatible dependency pins, minimum macOS/tool versions, storage bindings, managed worktree paths, watcher integration, local Git credential behavior, and a concrete GitHub capability inventory. The recommendations in this document make those tasks reviewable without presenting them as completed decisions or reopening the interview.
