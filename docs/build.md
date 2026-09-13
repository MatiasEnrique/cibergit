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
- Command-Shift-D cycles Auto, Unified, and Side-by-side diff modes. Auto responds to window width; explicit modes do not.
- Command-Shift-I toggles pull-request details.
- Command-Shift-P opens the command palette.
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
CIBERGIT_SMOKE_APPEARANCE=light \
CIBERGIT_SMOKE_SECOND_PR=14398 \
CARGO_TARGET_DIR=/tmp/cibergit-native-target \
cargo run --locked --features ui-smoke -- \
  --repo cli/cli --account YOUR_GH_LOGIN --pr 9847

mkdir -p /absolute/path/to/evidence/dark
CIBERGIT_DATA_DIR=/tmp/cibergit-ui-smoke-store \
CIBERGIT_SMOKE_DIR=/absolute/path/to/evidence/dark \
CIBERGIT_SMOKE_APPEARANCE=dark \
CIBERGIT_SMOKE_EXPECT_RESTORE=1 \
CIBERGIT_SMOKE_SECOND_PR=14398 \
CARGO_TARGET_DIR=/tmp/cibergit-native-target \
cargo run --locked --features ui-smoke -- \
  --repo cli/cli --account YOUR_GH_LOGIN --pr 9847
```

Each evidence directory receives `native-pr-review.png` and `native-pr-smoke.txt`. The harness labels these as programmatic native actions: the in-process capture does not prove physical keyboard/mouse input, Accessibility behavior, or the composited macOS blur behind the transparent sidebar. It performs no remote writes or destructive local editing.

## Native dependency notes

`gpui-base = 0.6.1` supplies inputs and safe Markdown text views. `gpui-pre = 0.3.1` and `gpui-pre-platform = 0.3.1` select one compatible GPUI family, with transitive resolutions retained in `Cargo.lock`. The `font-kit` feature renders glyphs, and `runtime_shaders` compiles Metal source through the system runtime. The full Xcode application and standalone Metal compiler are not required for this development path; a precompiled-shader distribution build does require them.
