# Building cibergit

cibergit is a native pull-request review workspace for Apple Silicon Macs. Comparisons remain immutable until explicitly advanced; review and merge mutations are opt-in, confirmed where appropriate, and executed only through the selected account's provider session.

## Requirements

- Apple Silicon Mac, macOS 15 or newer. Validated host: macOS 26.5.2.
- Rust 1.97.1 and Cargo.
- Apple Command Line Tools for native compilation.
- Git and an authenticated GitHub CLI (`gh`). Account discovery and every remote read use the selected `gh` login.

Build and launch from the repository:

```sh
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo build --locked
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo run --locked
```

Keep each worktree's writable target directory isolated. A copied dependency seed is safe, but multiple worktrees must not share the same writable `CARGO_TARGET_DIR`.

## Review workspace startup

Open a specific public or accessible pull request:

```sh
cargo run --locked -- \
  --repo cli/cli \
  --account YOUR_GH_LOGIN \
  --pr 9847
```

`--repo` accepts `owner/name`, a GitHub URL, or a local repository folder. Direct repository startup requires `--account`; the native setup screen can discover and select an account interactively. Local repositories use a three-dot full-PR comparison. The comparison is pinned until the user explicitly advances to an observed new head.

Workspace state normally uses the platform application-data directory. Isolate a run without touching the personal workspace with either form:

```sh
cargo run --locked -- --data-dir /tmp/cibergit-data
CIBERGIT_DATA_DIR=/tmp/cibergit-data cargo run --locked
```

Unreadable, foreign, or future-version workspace/session data is reported and preserved. The application does not replace it with defaults on disk.

## Local editing from a pull request

Choose **Edit locally** (Command-Shift-E) in a PR tab. Create a dedicated checkout
at the displayed review commit, or verify and attach an existing checkout. The
selected file opens in the focused editor. The published comparison keeps its
commit and selection while you work locally. Return with **Published review**
(Command-Option-Shift-E); Command-Shift-R remains review submission.

A saved association reopens its existing checkout after restart. **Reconcile
interrupted setup** checks actual Git and filesystem identity without replaying
an interrupted operation. Incomplete setup files remain preserved for recovery.
Initial remote provisioning and explicit object fetch have a three-minute deadline;
local Git actions retain their separate shorter bound. Git network credentials
and commit authorship come from the installed Git configuration.

File saves use the conflict-aware document store and preserve recovery data and
retained previous file versions. The old `--edit PATH` prototype no longer writes
files; it opens a compatibility screen directing you to the PR workspace.

## Native review controls

- Command-R refreshes reads; focus also triggers a refresh. Polling backs off after failures and slows while inactive.
- Command-] and Command-[ select the next and previous changed file.
- Click a directory chevron in the changed-file tree to collapse or expand it. Click the tree,
  then use Up/Down to move through visible rows, Left/Right to collapse/expand or move between
  parents and children, and Return to activate the focused row. Next/previous file navigation
  follows the complete comparison order even when a destination directory is collapsed; its
  ancestors are expanded and the selected row is revealed.
- Command-Shift-D cycles Auto, Unified, and Side-by-side diff modes. Auto responds to window width; explicit modes do not.
- Diff lines scroll horizontally with the trackpad/native scrollbar. After clicking the diff,
  Left/Right scroll by one keyboard step and Home/End reach its horizontal edges. Line numbers
  and change markers remain aligned in unified and side-by-side modes. Tabs use stable four-column
  stops; exceptionally long lines are split into bounded UTF-8-safe shaping chunks without
  removing source text.
- Command-Shift-I toggles pull-request details.
- Click a selectable diff line to open an inline composer; Shift-click another line on the same
  side extends the exact range. Press `c` with the diff focused to choose the first selectable
  line. Text changes are locally autosaved after a short debounce; Command-Return forces a local
  restart/offline recovery save. Command-Shift-Return explicitly adds
  the saved text to the selected account's pending review, and Command-Option-Return explicitly
  posts it immediately. No comment action submits a review implicitly.
- Activity keeps the selected user's pending review separate from historical review and issue
  discussion. Linked pending comments can be reopened and edited; browser-created comments that
  lack an exact local recovery link stay authoritative and are identified instead of guessed into
  local state. Replies, resolve/unresolve, pending-summary updates, comment deletion and review
  cancellation use their visible explicit controls. Partial provider reads are labelled.
- Command-Shift-R opens native review submission confirmation. Choose Comment, Approve, or Request
  changes and confirm the summary, pending count, account and exact reviewed head. If a newer head
  exists, submission remains bound to the older displayed head and shows that warning.
- Command-Shift-M performs a fresh merge preflight and then opens native confirmation with the
  repository, pull request, account, reviewed/current heads, rule/check/review blockers, supported
  merge methods and real auto-merge/queue choices. Confirmation dispatches one guarded request.
  Enable/queue acknowledgement is displayed as enabled/queued, not merged. Branch deletion is
  unavailable because the provider has no CAS-safe delete-ref operation; there is no working-looking
  checkbox or automatic administrator bypass.
- Drag the dividers beside the repository sidebar, changed-file tree, and details pane to resize
  them. Command-Shift-B and Command-Shift-F collapse/restore the sidebar and file tree. Control-
  Option-Left/Right adjusts the sidebar; add Shift for the file tree and Command for details.
  Control-Option-0, or **Reset panel layout** in the command palette, restores the native defaults.
  Panel sizes are session-local in this milestone.
- Command-Shift-P opens the command palette.
- Open **Edit view…** in the sidebar (or choose **Edit sidebar filters and grouping** in the
  command palette) to compose filters and ordered grouping levels. Text fields accept exact
  GitHub values; source grouping can use an exact branch or a configurable prefix. Apply with
  Command-S, cancel with Escape, or use the visible buttons. **Save as new** requires a name;
  applying a changed name renames the selected view, and **Delete view** always leaves a valid
  default or another saved view selected.
- Command-W closes the active review tab. Command-O opens repository setup.
- File progress, per-file scroll, comparison mode, pinned revision, and tab state persist in the selected data directory.

The displayed `ReviewSession` remains authoritative for the immutable comparison and revision;
independent metadata, comments, checks and pending-review polls never advance it. Review text is
partitioned by account/repository/PR and saved in private, versioned atomic recovery files. A
failed initial save sends zero provider writes. Started or uncertain review operations survive
restart, freeze incompatible preparation, trigger reads only, and are never replayed blindly.
Auxiliary and merge operations additionally write an exact caller-owned request/attempt journal
before dispatch under a crash-released per-review lock. Corrupt/future recovery is preserved.
Authentication, capability, mapping and persistence failures keep local text and are disclosed.

## Validation

```sh
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo fmt --check
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo check --locked --all-targets --all-features
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo clippy --locked --all-targets --all-features -- -D warnings
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo test --locked --all-targets --all-features
python3 scripts/dependency-notices.py
```

`THIRD_PARTY_NOTICES.md` retains resolved dependency license metadata and package license/notice files. Cargo reports a future Rust incompatibility in the transitive `block 0.1.6`; it does not fail the verified toolchain build.

## Opt-in real-read UI smoke

The `ui-smoke` feature waits for an actual comparison, sidebar list, and details response before capturing the application-owned Metal scene. With a multi-file primary PR and a second PR, it invokes the same native handlers used by the UI and asserts next/previous file selection, tab switch/restore, explicit diff mode across resize, independently sequenced two-tab saves, and persisted-session read-back.

```sh
test ! -e /tmp/cibergit-ui-smoke-store
mkdir -p /absolute/path/to/evidence/light
CIBERGIT_DATA_DIR=/tmp/cibergit-ui-smoke-store \
CIBERGIT_SMOKE_DIR=/absolute/path/to/evidence/light \
CIBERGIT_SMOKE_BACKGROUND=1 \
CIBERGIT_SMOKE_APPEARANCE=light \
CIBERGIT_SMOKE_SECOND_PR=14398 \
CARGO_TARGET_DIR=/tmp/cibergit-native-target \
cargo run --locked --features ui-smoke -- \
  --repo cli/cli --account YOUR_GH_LOGIN --pr 9847

mkdir -p /absolute/path/to/evidence/dark
CIBERGIT_DATA_DIR=/tmp/cibergit-ui-smoke-store \
CIBERGIT_SMOKE_DIR=/absolute/path/to/evidence/dark \
CIBERGIT_SMOKE_BACKGROUND=1 \
CIBERGIT_SMOKE_APPEARANCE=dark \
CIBERGIT_SMOKE_EXPECT_RESTORE=1 \
CIBERGIT_SMOKE_SECOND_PR=14398 \
CARGO_TARGET_DIR=/tmp/cibergit-native-target \
cargo run --locked --features ui-smoke -- \
  --repo cli/cli --account YOUR_GH_LOGIN --pr 9847
```

`CIBERGIT_SMOKE_BACKGROUND=1` is an opt-in capture mode: its native window is
created without focus and the application does not request activation. Normal
launches retain the standard foreground activation behavior.

Each evidence directory receives `native-pr-review.png`,
`native-review-interactions-unified.png`, `native-review-interactions-split.png`,
`native-submit-confirmation.png`, `native-merge-confirmation.png`,
`native-merge-confirmation-controls.png`, `native-long-line-end*.png`
captures, `native-long-line-start-split.png`, `native-view-editor-filters.png`,
`native-view-editor-groups.png`, and `native-pr-smoke.txt`. The fresh Auto run also writes
`native-pr-review-split.png` before applying an explicit override.
The harness labels these as
programmatic native actions: the in-process captures do not prove physical keyboard/mouse input,
Accessibility behavior, or the composited macOS blur behind the transparent sidebar. It performs
no remote writes or destructive local editing.

The native assertions measure the rendered diff viewport (after all panels), exercise a wide Auto
split and narrow Auto unified layout, preserve an explicit override through resize/tab restore and
restart, collapse and reveal a destination directory, adjust/reset a splitter, and scroll a unique
long-line token to the far end in both unified and split modes before capture. Split source regions
are independently clipped beneath fixed OLD/NEW headers and gutters while sharing one native
horizontal offset. Read the distinct OLD/NEW sentinels in both `native-long-line-start-split.png`
and `native-long-line-end-split.png`; checking only the scroll offset is not accepted as visual
evidence. The real `cli/cli#14130` Overview capture also exercises narrow selectable Markdown around
“Issue fields are not currently…”. In pinned GPUI Base 0.6.1, an inline-code mark switches the whole
paragraph to a fragment flow whose wrapped origins can overlap at narrow widths. cibergit escapes
inline-code delimiters into literal selectable backticks as a bounded fallback; exact visible text,
links, fenced code blocks, and media-free rendering remain intact, while inline code does not receive
special code styling. The review-interaction captures use a real displayed patch and the actual
line-selection/controller/render handlers to place two wrapped thread rows and a focused multiline
composer between their exact source rows. They resize to the minimum-width scene, reach the shared
horizontal end, and assert measured variable row heights and non-overlap in both unified and split
modes. Their Markdown fixture verifies that media is removed from prose while fenced and indented
code preserve literal backticks, HTML comments/tags, angle brackets and image-looking text.

The first smoke run also saves a named view with an exact source-branch filter and global
target → repository → source-prefix grouping. The restart run requires that exact view and the
review session to restore, then reads the persisted view back before reporting success. Use the
same isolated data directory for the two runs and remove it before starting a new smoke pair.

## Native dependency notes

`gpui-base = 0.6.1` supplies inputs and safe Markdown text views. `gpui-pre = 0.3.1` and `gpui-pre-platform = 0.3.1` select one compatible GPUI family, with transitive resolutions retained in `Cargo.lock`. The `font-kit` feature renders glyphs, and `runtime_shaders` compiles Metal source through the system runtime. The full Xcode application and standalone Metal compiler are not required for this development path; a precompiled-shader distribution build does require them.

## Local app bundle

Create an Apple Silicon `.app` with a local ad-hoc signature:

```sh
./scripts/package-app.sh --output target/package/cibergit-dev.app
```

The command builds the default development profile, honors `CARGO_TARGET_DIR`,
includes license notices and build provenance, verifies the arm64 executable and
bundle signature, and writes a file-hash manifest beside the bundle. The output
path must be new; an existing app is never replaced. Use `--release` for an
optimized build or `--ui-smoke` to include the opt-in background native capture
harness. Normal bundles omit that harness.

This ad-hoc signature uses no signing identity or credentials. It does not provide
Developer ID distribution signing, notarization, or validation on macOS 15. Those
remain separate release gates.
