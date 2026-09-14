# Navigation performance

Run an optimized local build for everyday review:

```sh
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo run --release --locked
```

The PR sidebar uses a virtual list. Its grouping and filtering cache is keyed by the full repository/account, immutable PR inventory, and saved view. A same-length refresh, account change, or filter change rebuilds the model; scrolling and selection reuse it. Only visible PR rows construct controls and resolve selection/unread state.

The file tree shares flattened rows and an identity index until its inventory or expanded directories change. Viewed markers are read for the requested viewport. Diff rows and text-width measurements are reused until the diff changes. Review-session snapshots share immutable comparison bytes; loading a patch uses copy-on-write, while selection and review progress remain per session. Serialized comparison fields are unchanged. A nonserialized file index avoids scanning all paths on every selected-file lookup.

The native regression `large_sidebar_virtualizes_rows_and_reuses_diff_and_tree_on_redraw` exercises 5,001 PRs, 5,000 files, and 20,000 diff rows. It checks bounded sidebar materialization, reaches and clicks the final PR/file rows, and checks shared row buffers across redraws. This is a structural/interaction regression, not an FPS guarantee.

The snapshot microbenchmark runs separately in release mode:

```sh
CARGO_TARGET_DIR=/tmp/cibergit-native-target cargo test --release --locked --test review_session benchmark_navigation_snapshot_copy_vs_shared_comparison -- --ignored --nocapture
```

It compares full comparison copies with shared session snapshots for 500 files containing approximately 16 MiB of patch text. It measures allocation/copy cost only, excluding network latency, patch parsing, disk persistence, and rendering.
