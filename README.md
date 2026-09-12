# cibergit

cibergit is a local Git client for developers reviewing pull requests, including work produced by coding agents. It brings PR diffs, discussions, checks, local changes, source editing, and interactive rebase into one desktop application.

The initial target is macOS on Apple Silicon, with GitHub.com integration through the GitHub CLI. The application uses Rust, GPUI, and GPUI Kit. Cibergit's own code will use the MIT license.

This repository currently contains the design documentation. Application implementation has not started.

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
