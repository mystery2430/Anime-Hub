#!/usr/bin/env python3
"""Copy the Kotlin bridge into a generated Tauri Android project and patch PiP.

`tauri android init` rewrites `src-tauri/gen/android/` and does not know about
this repo's plugin. Running this afterwards is what makes the Keystore / PiP
class and `android:supportsPictureInPicture` actually part of the build.

The generated package name is checked against the string passed to
`register_android_plugin` in `android-plugin/src/lib.rs`. Those two drifting
apart is exactly how the bridge fails to load, so this script refuses to
paper over a mismatch.

Usage:
    python3 scripts/android_prepare.py [--root DIR] [--gen DIR]
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

PIP_ATTR = 'android:supportsPictureInPicture="true"'
SIGN_MARKER = "AnimeHub release signing"
# Tauri v2's generated Android template has no signing config at all, so a
# release build is always unsigned unless we inject one. The block is a
# no-op when keystore.properties is absent (dev builds stay unsigned).
SIGN_BLOCK = """
// >>> %s (injected by scripts/android_prepare.py) >>>
android {
    val ksFile = rootProject.file("keystore.properties")
    if (ksFile.exists()) {
        val ks = java.util.Properties()
        ksFile.inputStream().use { ks.load(it) }
        signingConfigs.create("release") {
            storeFile = java.io.File(ks.getProperty("storeFile"))
            storePassword = ks.getProperty("storePassword")
            keyAlias = ks.getProperty("keyAlias")
            keyPassword = ks.getProperty("keyPassword")
        }
        buildTypes.getByName("release").signingConfig =
            signingConfigs.getByName("release")
    }
}
// <<< %s <<<
""" % (SIGN_MARKER, SIGN_MARKER)
DEEP_LINK = """        <intent-filter>
            <action android:name="android.intent.action.VIEW" />
            <category android:name="android.intent.category.DEFAULT" />
            <category android:name="android.intent.category.BROWSABLE" />
            <data android:scheme="animehub" android:host="anilist" android:pathPrefix="/callback" />
        </intent-filter>
"""
REGISTER_RE = re.compile(
    r'register_android_plugin\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*\)'
)
PACKAGE_RE = re.compile(r"^package\s+(\S+)", re.MULTILINE)


def rust_registration(lib_rs: Path) -> tuple[str, str]:
    text = lib_rs.read_text(encoding="utf-8")
    match = REGISTER_RE.search(text)
    if not match:
        raise SystemExit(f"register_android_plugin(...) not found in {lib_rs}")
    return match.group(1), match.group(2)


def find_main_activity(gen: Path) -> Path:
    matches = sorted(gen.glob("app/src/main/java/**/MainActivity.kt"))
    # pathlib ** does not cross with glob the way people expect on older
    # Pythons unless the pattern is passed to rglob.
    if not matches:
        matches = sorted(gen.rglob("MainActivity.kt"))
    if not matches:
        raise SystemExit(f"MainActivity.kt not found under {gen}; run tauri android init")
    return matches[0]


def package_of(path: Path) -> str:
    text = path.read_text(encoding="utf-8")
    match = PACKAGE_RE.search(text)
    if not match:
        raise SystemExit(f"no package declaration in {path}")
    return match.group(1).rstrip(";")


def ensure_pip(xml: str) -> str:
    if "supportsPictureInPicture" in xml:
        return xml
    updated, n = re.subn(r"<activity\b", f"<activity\n        {PIP_ATTR}", xml, count=1)
    if n != 1:
        raise SystemExit("AndroidManifest.xml has no <activity> element to mark for PiP")
    return updated


def ensure_deep_link(xml: str) -> str:
    if 'android:scheme="animehub"' in xml or "android:scheme='animehub'" in xml:
        return xml
    if "</activity>" not in xml:
        raise SystemExit("AndroidManifest.xml has no </activity> to attach the OAuth filter to")
    return xml.replace("</activity>", DEEP_LINK + "    </activity>", 1)


def ensure_proguard(path: Path, package: str, class_name: str) -> None:
    rule = (
        f"-keep class {package}.{class_name} {{ *; }}\n"
        "-keepclassmembers class * {\n"
        "    @app.tauri.annotation.Command <methods>;\n"
        "}\n"
    )
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    marker = f"-keep class {package}.{class_name}"
    if marker in existing:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as fh:
        if existing and not existing.endswith("\n"):
            fh.write("\n")
        fh.write(rule)


def prepare(root: Path, gen: Path) -> list[str]:
    notes: list[str] = []
    lib_rs = root / "src-tauri" / "android-plugin" / "src" / "lib.rs"
    package, class_name = rust_registration(lib_rs)

    kt_src = root / "src-tauri" / "android-plugin" / "kotlin" / Path(
        package.replace(".", "/")
    ) / f"{class_name}.kt"
    if not kt_src.is_file():
        raise SystemExit(f"Kotlin bridge not found at {kt_src}")
    kt_pkg = package_of(kt_src)
    if kt_pkg != package:
        raise SystemExit(
            f"Kotlin package {kt_pkg!r} != register_android_plugin package {package!r}"
        )

    activity = find_main_activity(gen)
    gen_pkg = package_of(activity)
    if gen_pkg != package:
        raise SystemExit(
            "generated MainActivity package does not match register_android_plugin.\n"
            f"  generated: {gen_pkg}\n"
            f"  expected:  {package}\n"
            "Update the Kotlin `package` line and the Rust registration together, "
            "then re-run this script."
        )
    notes.append(f"package {package} matches MainActivity")

    dest = gen / "app" / "src" / "main" / "java" / Path(package.replace(".", "/"))
    dest.mkdir(parents=True, exist_ok=True)
    target = dest / f"{class_name}.kt"
    target.write_text(kt_src.read_text(encoding="utf-8"), encoding="utf-8")
    notes.append(f"copied {class_name}.kt -> {target}")

    manifests = list(gen.glob("app/src/main/AndroidManifest.xml"))
    if not manifests:
        manifests = [p for p in gen.rglob("AndroidManifest.xml") if "build" not in p.parts]
    if not manifests:
        raise SystemExit(f"AndroidManifest.xml not found under {gen}")
    manifest = manifests[0]
    original = manifest.read_text(encoding="utf-8")
    updated = ensure_deep_link(ensure_pip(original))
    if updated != original:
        manifest.write_text(updated, encoding="utf-8")
        notes.append(f"patched {manifest}")
    else:
        notes.append(f"manifest already had PiP + animehub scheme ({manifest})")

    proguard = gen / "app" / "proguard-rules.pro"
    ensure_proguard(proguard, package, class_name)
    notes.append(f"proguard keep for {package}.{class_name}")

    gradle = gen / "app" / "build.gradle.kts"
    text = gradle.read_text(encoding="utf-8")
    if SIGN_MARKER in text:
        notes.append(f"signing config already present in {gradle}")
    else:
        gradle.write_text(text + SIGN_BLOCK, encoding="utf-8")
        notes.append(f"injected release signing config into {gradle}")
    return notes


def main() -> int:
    here = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=here.parent)
    parser.add_argument("--gen", type=Path, default=None)
    args = parser.parse_args()
    root = args.root.resolve()
    gen = (args.gen if args.gen is not None else root / "src-tauri" / "gen" / "android").resolve()
    if not gen.is_dir():
        print(f"android_prepare: {gen} does not exist; run `tauri android init` first", file=sys.stderr)
        return 1
    for note in prepare(root, gen):
        print(note)
    return 0


if __name__ == "__main__":
    sys.exit(main())
