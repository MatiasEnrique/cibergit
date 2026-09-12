# Building cibergit

The implementation is in progress. The current development foundation opens a native GPUI window with GPUI Kit’s focused editor. It is not yet a PR client or a release build.

## Requirements

- Apple Silicon Mac, macOS 15 or newer. Validated host: macOS 26.5.2.
- Rust 1.97.1 and Cargo (the verified toolchain).
- Apple Command Line Tools for native compilation.
- Installed Git and GitHub CLI (`gh`) for the forthcoming repository workflows.

Run from the repository:

```sh
cargo build --locked
cargo run --locked -- /absolute/path/to/a/disposable-text-file.rs
```

The initial window supports editing an existing UTF-8 file and saving with the Save button or Command-S. Use disposable files for this foundation preview: full external-writer reconciliation and recovery are not implemented yet. Command-Q quits.

## Pinned native dependencies

`gpui-base =0.6.1` supplies the GPUI Kit focused editor. `gpui-pre =0.3.1` and `gpui-pre-platform =0.3.1` select one compatible family, with transitive resolutions retained in Cargo.lock. The `font-kit` feature renders glyphs; `runtime_shaders` compiles the Metal source through the system Metal runtime. This upstream-supported development path avoids invoking the standalone `metal` and `metallib` executables. It does not replace GPUI or its Metal renderer.

The full Xcode application and standalone Metal compiler are absent on the validation host. A precompiled-shader distribution build requires those tools. No global developer-directory settings have been changed.

Upstream references: [Kit manifest](https://github.com/longbridge/gpui-kit/blob/main/Cargo.toml), [focused editor](https://gpui-kit.com/base/primitives/editor/), [GPUI platform package](https://crates.io/crates/gpui-pre-platform/0.3.1).

## Validation and notices

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 scripts/dependency-notices.py
```

`THIRD_PARTY_NOTICES.md` retains resolved dependency license metadata and package license/notice files. Original cibergit code is MIT. Zed’s GPL editor crate is not used. Cargo reports a future Rust incompatibility in the transitive `block 0.1.6`; it does not fail the verified toolchain build.

The optional `ui-smoke` feature captures the application’s own Metal-rendered scene and exercises the editor/save path without granting desktop-wide screen capture or accessibility access:

```sh
mkdir -p .coordination/evidence
printf '// smoke input\n' > .coordination/evidence/editor-smoke.rs
CIBERGIT_SMOKE_DIR="$PWD/.coordination/evidence" cargo run --locked --features ui-smoke -- .coordination/evidence/editor-smoke.rs
```

This application-owned capture does not establish physical keyboard/mouse or accessibility behavior. Those checks remain separate.
