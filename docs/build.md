# Building cibergit

cibergit is a native, read-only pull-request review workspace for Apple Silicon Macs. The default launch opens repository setup; a reproducible direct launch can select a GitHub account, repository, and pull request from the command line.

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

The original entity editor remains available explicitly:

```sh
cargo run --locked -- --edit /absolute/path/to/file.rs
```

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

The M1 workspace performs GitHub and local repository reads only. Review/check state remains `UNKNOWN` when unavailable; it is never inferred from missing data. Drafting, publishing, and destructive local editing are outside this smoke path.

## Validation

```sh
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo fmt --check
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo check --locked --all-targets --features ui-smoke
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo clippy --locked --all-targets --features ui-smoke -- -D warnings
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo test --locked
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

Each evidence directory receives `native-pr-review.png`, unified and split
`native-long-line-end*.png` captures, `native-long-line-start-split.png`, `native-view-editor-filters.png`,
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
special code styling.

The first smoke run also saves a named view with an exact source-branch filter and global
target → repository → source-prefix grouping. The restart run requires that exact view and the
review session to restore, then reads the persisted view back before reporting success. Use the
same isolated data directory for the two runs and remove it before starting a new smoke pair.

## Native dependency notes

`gpui-base = 0.6.1` supplies inputs and safe Markdown text views. `gpui-pre = 0.3.1` and `gpui-pre-platform = 0.3.1` select one compatible GPUI family, with transitive resolutions retained in `Cargo.lock`. The `font-kit` feature renders glyphs, and `runtime_shaders` compiles Metal source through the system runtime. The full Xcode application and standalone Metal compiler are not required for this development path; a precompiled-shader distribution build does require them.
