#!/usr/bin/env python3
"""Build a local Apple Silicon app bundle; never use distribution credentials."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tempfile


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def rename_exclusive(source, destination):
    # Darwin sys/stdio.h: RENAME_EXCL, including an output created concurrently.
    libc = ctypes.CDLL(None, use_errno=True)
    rename = libc.renamex_np
    rename.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint]
    rename.restype = ctypes.c_int
    if rename(os.fsencode(source), os.fsencode(destination), 0x00000004) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(destination))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", action="store_true", help="Build the release profile")
    parser.add_argument("--ui-smoke", action="store_true", help="Include opt-in native capture support")
    parser.add_argument("--output", type=Path, help="New .app path; existing output is never replaced")
    args = parser.parse_args()
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        parser.error("packaging requires an Apple Silicon Mac")
    root = Path(__file__).resolve().parent.parent
    os.chdir(root)
    metadata = json.loads(run("cargo", "metadata", "--locked", "--no-deps", "--format-version", "1", capture_output=True).stdout)
    package = next(p for p in metadata["packages"] if Path(p["manifest_path"]) == root / "Cargo.toml")
    output = (args.output or Path(metadata["target_directory"]) / "package/cibergit.app").absolute()
    report = output.with_suffix(".manifest.json")
    if output.suffix != ".app" or output.exists() or output.is_symlink() or report.exists():
        parser.error("--output must name a new .app path; existing output is preserved")
    command = ["cargo", "build", "--locked", "--target", "aarch64-apple-darwin", "--bin", "cibergit", "--message-format=json-render-diagnostics"]
    if args.release:
        command.append("--release")
    if args.ui_smoke:
        command.extend(["--features", "ui-smoke"])
    built = run(*command, stdout=subprocess.PIPE)
    artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith("{")]
    executable = next(Path(item["executable"]) for item in reversed(artifacts)
                      if item.get("reason") == "compiler-artifact" and item.get("executable")
                      and item["target"]["name"] == "cibergit")
    architectures = run("lipo", "-archs", str(executable), capture_output=True).stdout.strip()
    if architectures != "arm64":
        raise RuntimeError(f"unexpected executable architecture: {architectures}")
    revision = run("git", "rev-parse", "HEAD", capture_output=True).stdout.strip()
    dirty = bool(run("git", "status", "--porcelain", "--untracked-files=no", capture_output=True).stdout.strip())
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".cibergit-package-", dir=output.parent) as temporary:
        staging = Path(temporary) / "cibergit.app"
        contents = staging / "Contents"
        resources = contents / "Resources"
        (contents / "MacOS").mkdir(parents=True)
        resources.mkdir()
        shutil.copy2(executable, contents / "MacOS/cibergit")
        for source in ("LICENSE", "THIRD_PARTY_NOTICES.md", "assets/fonts/Geist-LICENSE.txt",
                       "assets/fonts/Inter-LICENSE.txt", "assets/icons/cibergit.icns"):
            shutil.copy2(root / source, resources / Path(source).name)
        info = {
            "CFBundleExecutable": "cibergit", "CFBundleIdentifier": "dev.cibergit.cibergit",
            "CFBundleName": "cibergit", "CFBundleDisplayName": "cibergit",
            "CFBundleIconFile": "cibergit.icns",
            "CFBundlePackageType": "APPL", "CFBundleShortVersionString": package["version"],
            "CFBundleVersion": "1", "LSMinimumSystemVersion": "15.0",
            "LSApplicationCategoryType": "public.app-category.developer-tools",
            "NSHighResolutionCapable": True, "NSPrincipalClass": "NSApplication",
        }
        with (contents / "Info.plist").open("wb") as stream:
            plistlib.dump(info, stream)
        build_info = {"revision": revision, "tracked_changes": dirty,
                      "profile": "release" if args.release else "dev", "ui_smoke": args.ui_smoke,
                      "architecture": architectures, "signing": "local ad-hoc; not notarized"}
        (resources / "build-info.json").write_text(json.dumps(build_info, indent=2) + "\n")
        run("plutil", "-lint", str(contents / "Info.plist"))
        # '-' is an ad-hoc signature; this never selects a user's signing identity.
        run("codesign", "--force", "--sign", "-", str(staging))
        run("codesign", "--verify", "--deep", "--strict", str(staging))
        rename_exclusive(staging, output)
    manifest = [{"path": str(p.relative_to(output)), "size": p.stat().st_size,
                 "sha256": hashlib.sha256(p.read_bytes()).hexdigest()}
                for p in sorted(output.rglob("*")) if p.is_file()]
    with report.open("x") as stream:
        json.dump({"build": build_info, "files": manifest}, stream, indent=2)
        stream.write("\n")
    print(f"Local ad-hoc development app: {output}")
    print(f"Verified manifest: {report}")
    print("Distribution signing, notarization and oldest-supported-OS validation remain separate.")


if __name__ == "__main__":
    main()
