# Release profile

Build an optimized local Apple Silicon bundle with:

```sh
CARGO_BUILD_JOBS=2 ./scripts/package-app.sh --release --output /absolute/new/path/cibergit.app
```

Use a new output path. The package script refuses to overwrite existing output. `CARGO_TARGET_DIR` can isolate compilation. The local signature is ad-hoc; Developer ID and notarization are separate release requirements.

The pinned gpui-pre-macros 0.3.1 crate contains a helper that references its inspector module even when that module is disabled in release mode. Cargo.toml enables debug assertions only for this host proc-macro package. Application and runtime crates retain the usual optimized release configuration, with no application debug assertions or smoke feature. This uses Cargo's documented [package-specific profile overrides](https://doc.rust-lang.org/cargo/reference/profiles.html#overrides). Dependency versions, features, lockfile and registry source remain unchanged.

The first optimized checkpoint was built from clean source `f3cec8ba61c1e8e9bce5090f8cffc4f01b89ec84`; its profile correction is integrated as `a401860e7310f9d5daa8900cea385495f14e4622`. Extracted bundle files matched the manifest, strict ad-hoc signature verification passed, and the binary reported arm64 with macOS 15.0 minimum deployment metadata. CLI help and application cfg inspection passed. Evidence is stored in artifact `art_5d45f3c0-34d0-45fb-950b-91159d7abc51`, version `av_7ca15477-7651-4262-ad5c-804d46cd2799`.

That checkpoint contains a known native reconciliation defect being corrected and is not a V1 release candidate. Its validation establishes compilation, package integrity, architecture and CLI behavior. It does not establish macOS 15 runtime compatibility, Finder/Gatekeeper distribution, physical input, desktop acrylic compositing, final icon, performance or completion of the feature inventory. Rebuild and validate the final integrated source before distribution.

## App icon

The cibergit mark is bundled as `Contents/Resources/cibergit.icns` and declared by `CFBundleIconFile`. The artwork is `assets/icons/cibergit.svg`, the same vector the ciber marketing site uses; the committed PNG is a 1024-pixel preview. `scripts/render-app-icon.swift` reads that SVG, fills its outlines onto the icon tile with AppKit, and hands the set to `iconutil`. To regenerate all ten standard icon representations without replacing existing output:

```sh
swift scripts/render-app-icon.swift /tmp/cibergit-icon-new
```

Rendering uses system AppKit and `iconutil`, with no build dependency beyond the committed SVG. The script reads absolute moves, lines and cubics and refuses any other path command rather than approximating it, so replacing the SVG either renders faithfully or fails loudly. The mark and the generated artwork use cibergit's MIT license.
