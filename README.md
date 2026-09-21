# cibergit

cibergit is a local Git client for developers reviewing pull requests, including work produced by coding agents. It brings PR diffs, discussions, checks, local changes, source editing, and interactive rebase into one desktop application.

The target is macOS on Apple Silicon, with GitHub.com integration through the GitHub CLI. The application uses Rust, GPUI, and GPUI Kit. Cibergit's own code uses the MIT license.

Implementation is underway. The native application can open real PRs, display pinned diffs and collaboration details, and restore review tabs and file progress. The complete V1 scope remains in progress; see the [implementation ledger](docs/implementation-ledger.md) for acceptance status.

## Current app

The review workspace has Conversation, Commits, Checks, and Files changed tabs on one header bar, a resizable repository sidebar, and a display menu carrying the reading mode and the Unified / Side by side choice. A comparison opens as one continuous scroll over every changed file, with each file's own header, its own horizontal scrolling, and a strip down the right edge showing which file you are in. Each file folds from its header, one control folds or opens all of them, and one file at a time remains a choice. Large PR and file lists render visible rows instead of building every row on each redraw, and the conversation does the same once it has been measured: a review carrying long comments from a coding agent scrolls without reshaping every comment on the page for each frame.

This implementation also includes explicit selection among stack tips and confirmed Actions run controls for re-running all jobs, re-running failed jobs, or cancelling a run. See [stack navigation](docs/stack-ui.md), [Actions jobs and logs](docs/actions-jobs-logs.md), and [run controls](docs/actions-run-controls.md) for their behavior and limits.

A read-only History page shows the repository's own commit graph over every branch, with the selected commit's full diff beside it. Its commit column is resizable and collapses to a rail of lane-coloured dots, so the diff can have the width. See [commit history](docs/commit-history.md) for its scope chips, bounds, and what a repository with no local clone cannot show.

The review, History and Stack diffs are one pane with different things around it, and they read by keyboard: a cursor that moves by row, by hunk and by comment thread, plus one key that marks a file viewed and advances to the next one that is not. See [interface direction](docs/ui-direction.md) for the keys, for why they are modifier-based, and for what a comparison too large to stream does instead.

Full V1 remains unfinished. Stack-wide review and merge operations, automatic descendant rewriting, broader GitHub Actions management, and signed/notarized distribution are not complete.

## Documentation

- [Technical design](docs/technical-design.md): technology choices, provider integration, state ownership, local Git, synchronization, implementation milestones, and unresolved details.
- [Product requirements](docs/product-requirements.md): agreed behavior and scope, with references to the design interview.
- [Implementation coordinator prompt](docs/implementation-coordinator.md): BB worker orchestration, milestones, integration checks, and durable state for a long-running implementation thread.

## Initial scope

- Explicitly add repositories and review all open PRs by default.
- Compose sidebar groups by repository, target branch, source branch, and PR stack. Save grouping and filter combinations.
- Review individual PRs or the net remaining changes in a stack.
- Keep multiple PRs open in tabs, with synchronized discussions and checks.
- Review without cloning, then attach or create a worktree when local work is needed.
- Edit source files and observe changes made by external editors and agents.
- Stage, commit, push, resolve conflicts, and interactively rebase one branch at a time.

Git and `gh` are user-installed prerequisites. Cibergit has no hosted service. GitLab and other providers are future integrations; AI functionality is deferred to V2. Apple Silicon is the only committed hardware target.

## Decision status

The documentation distinguishes agreed requirements, verified upstream constraints, and proposed implementation details. The final interview round was not answered except for Apple Silicon coverage. Its remaining proposals are recorded as unresolved, not approved requirements.

## Development

See [build/run instructions](docs/build.md) for the native review workspace and reproducible smoke checks. Work follows [the coordinator brief](docs/implementation-coordinator.md); the initial scope above describes the intended V1, including features that are still being implemented.
