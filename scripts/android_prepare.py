#!/usr/bin/env python3
"""Copy the Kotlin bridge into a generated Tauri Android project and patch PiP.

`tauri android init` rewrites `src-tauri/gen/android/` and does not know about
this repo's plugin. Running this afterwards is what makes the Keystore / PiP
classes, the PiP lifecycle on the generated `MainActivity` and
`android:supportsPictureInPicture` actually part of the build.

Three things are generated into the app package:

1. `AnimeHubPlugin.kt` — copied verbatim from `android-plugin/kotlin/`.
2. `AnimeHubPipController.kt` — a template whose
   `__ANIMEHUB_PIP_CONTROLLER_JS__` placeholder is filled with the *same*
   `src-tauri/src/web/pip_controller.js` that Rust injects into site pages, so
   the WebView-side controller cannot drift between the two consumers.
3. the PiP lifecycle block inside the generated `MainActivity.kt`, delimited by
   markers and replaced on every run.

`src-tauri/gen/android` is generated: never edit those files by hand, re-run
this script instead.

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
# Kotlin template -> generated controller, and the JS that fills its placeholder.
PIP_CONTROLLER_CLASS = "AnimeHubPipController"
PIP_CONTROLLER_JS_REL = ("src-tauri", "src", "web", "pip_controller.js")
PIP_CONTROLLER_KT_REL = ("src-tauri", "android-plugin", "kotlin")
PIP_JS_PLACEHOLDER = "__ANIMEHUB_PIP_CONTROLLER_JS__"
# Markers around the block that is injected into the generated MainActivity.
PIP_BLOCK_BEGIN = "// >>> AnimeHub PiP lifecycle (injected by scripts/android_prepare.py) >>>"
PIP_BLOCK_END = "// <<< AnimeHub PiP lifecycle <<<"
MAIN_ACTIVITY_IMPORTS = (
    "import android.content.res.Configuration",
    "import android.os.Build",
    "import android.webkit.WebView",
)
MAIN_ACTIVITY_CLASS_RE = re.compile(r"class\s+MainActivity\b[^{]*\{")
IMPORT_RE = re.compile(r"^import\s+\S+\s*$", re.MULTILINE)
# Tauri v2's generated Android template has no signing config at all, so a
# release build is always unsigned unless we inject one. The block is a
# no-op when keystore.properties is absent (dev builds stay unsigned).
SIGN_BLOCK = """
// >>> %s (injected by scripts/android_prepare.py) >>>
// No java.* qualified references here: Gradle's Kotlin DSL script
// compiler rejects them (Unresolved reference: util/io). The template
// already imports java.util.Properties at the top; file() is Project's.
android {
    val ksFile = rootProject.file("keystore.properties")
    if (ksFile.exists()) {
        val ks = Properties()
        ksFile.inputStream().use { ks.load(it) }
        signingConfigs.maybeCreate("release").apply {
            storeFile = file(ks.getProperty("storeFile"))
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


def kotlin_source(root: Path, package: str, class_name: str) -> Path:
    """Path of a Kotlin source inside `android-plugin/kotlin/`, by package."""
    base = root.joinpath(*PIP_CONTROLLER_KT_REL)
    return base / Path(package.replace(".", "/")) / f"{class_name}.kt"


def read_pip_controller_js(root: Path) -> str:
    """The WebView-side PiP controller, checked for the Kotlin embeddings it feeds.

    The file is embedded verbatim inside a Kotlin raw string, where a dollar
    sign starts an interpolation and three quotes end the literal. Both would
    turn a compile error into something much harder to read, so fail here with
    a message that names the file.
    """
    js_path = root.joinpath(*PIP_CONTROLLER_JS_REL)
    if not js_path.is_file():
        raise SystemExit(f"PiP controller JS not found at {js_path}")
    js = js_path.read_text(encoding="utf-8")
    if "$" in js:
        raise SystemExit(
            f"{js_path} contains a dollar sign; the file is embedded in a Kotlin "
            "raw string where that interpolates. Use string concatenation."
        )
    if '"""' in js:
        raise SystemExit(
            f"{js_path} contains triple quotes; the file is embedded in a Kotlin "
            "raw string, which they would terminate."
        )
    return js


def render_pip_controller(template: Path, js: str) -> str:
    """Fill the JS placeholder of the controller template."""
    text = template.read_text(encoding="utf-8")
    if PIP_JS_PLACEHOLDER not in text:
        raise SystemExit(f"{template} has no {PIP_JS_PLACEHOLDER} placeholder")
    return text.replace(PIP_JS_PLACEHOLDER, js.strip())


def pip_lifecycle_block() -> str:
    """The Kotlin that MainActivity needs for controlled PiP entry/exit."""
    return f"""{PIP_BLOCK_BEGIN}
  // Controlled Picture-in-Picture lifecycle. The logic lives in
  // AnimeHubPipController; this Activity only decides when a preparation may
  // start and when the page has to be restored.
  //
  // PiP is never entered directly: the WebView is prepared (player-only view)
  // first, so every path below goes through
  // AnimeHubPipController.requestEnter() and waits for the JS answer.

  override fun onWebViewCreate(webView: WebView) {{
    super.onWebViewCreate(webView)
    // The Tauri/Wry WebView is the one the launcher and every site page share.
    AnimeHubPipController.attach(this, webView)
  }}

  /**
   * Android 11 (API 30)+ asks the Activity whether it wants PiP. Returning
   * true claims the callback — "the app received it", whether or not it acts
   * on it — which also stops the framework from falling back to the legacy
   * pause -> onUserLeaveHint -> resume dance.
   */
  override fun onPictureInPictureRequested(): Boolean {{
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return false
    if (!AnimeHubPipController.autoEnterEnabled) return true
    AnimeHubPipController.requestEnter(this, "pip-requested")
    return true
  }}

  /**
   * Android 8.0-10 (API 26-29) fallback: there is no
   * onPictureInPictureRequested below API 30, so the leave hint is the entry
   * point. From API 30 on the request callback above owns the flow, and this
   * path stays closed so the two can never both fire.
   */
  override fun onUserLeaveHint() {{
    super.onUserLeaveHint()
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) return
    if (!AnimeHubPipController.autoEnterEnabled) return
    AnimeHubPipController.requestEnter(this, "user-leave-hint")
  }}

  /**
   * Exit signal only: entering PiP is handled by requestEnter(), so this
   * callback is where the page is put back exactly as it was (styles,
   * classes, nothing removed, playback untouched).
   */
  override fun onPictureInPictureModeChanged(
    isInPictureInPictureMode: Boolean,
    newConfig: Configuration,
  ) {{
    super.onPictureInPictureModeChanged(isInPictureInPictureMode, newConfig)
    if (isInPictureInPictureMode) {{
      AnimeHubPipController.keepRenderingInPip(AnimeHubPipController.webView())
    }} else {{
      // Diagnostic: the report is read before restore() releases the view.
      AnimeHubPipController.showDebugReport(this, AnimeHubPipController.webView())
      AnimeHubPipController.restore(AnimeHubPipController.webView())
    }}
  }}

  override fun onPause() {{
    // WryActivity.onPause() pauses the WebView; in PiP it must keep rendering.
    super.onPause()
    if (isInPictureInPictureMode) {{
      AnimeHubPipController.keepRenderingInPip(AnimeHubPipController.webView())
    }}
  }}

  override fun onDestroy() {{
    AnimeHubPipController.detach(this)
    super.onDestroy()
  }}
  {PIP_BLOCK_END}"""


def ensure_imports(text: str, imports: tuple[str, ...]) -> str:
    """Add `import ...` lines that the injected block needs, once each."""
    missing = [line for line in imports if line not in text]
    if not missing:
        return text
    matches = list(IMPORT_RE.finditer(text))
    if matches:
        insert_at = matches[-1].end()
        return text[:insert_at] + "\n" + "\n".join(missing) + "\n" + text[insert_at:]
    # No imports at all (a hand-written stub): put them after the package line.
    package = PACKAGE_RE.search(text)
    if not package:
        raise SystemExit("MainActivity.kt has neither a package nor an import line")
    insert_at = package.end()
    return text[:insert_at] + "\n\n" + "\n".join(missing) + text[insert_at:]


def ensure_pip_lifecycle(activity: Path) -> str:
    """Install or refresh the PiP lifecycle block in the generated MainActivity.

    Idempotent: the block is delimited by markers, so a second run replaces the
    block with the current text instead of appending a duplicate override (two
    `onDestroy` overrides would not compile).
    """
    text = activity.read_text(encoding="utf-8")
    block = pip_lifecycle_block()
    if PIP_BLOCK_BEGIN in text and PIP_BLOCK_END in text:
        start = text.index(PIP_BLOCK_BEGIN)
        end = text.index(PIP_BLOCK_END, start) + len(PIP_BLOCK_END)
        if text[start:end] == block:
            return f"PiP lifecycle already current in {activity}"
        activity.write_text(text[:start] + block + text[end:], encoding="utf-8")
        return f"refreshed the PiP lifecycle block in {activity}"
    match = MAIN_ACTIVITY_CLASS_RE.search(text)
    if not match:
        raise SystemExit(
            f"{activity} has no `class MainActivity ... {{` to attach the PiP "
            "lifecycle to. Re-run `tauri android init`."
        )
    if "TauriActivity" not in match.group(0):
        # onWebViewCreate()/the WebView lifecycle only exist on the Tauri/Wry
        # activity; a different base class means the generated project changed.
        raise SystemExit(
            f"{activity} does not extend TauriActivity ({match.group(0).strip()}); "
            "the injected PiP lifecycle would not compile"
        )
    updated = text[: match.end()] + "\n" + block + text[match.end():]
    updated = ensure_imports(updated, MAIN_ACTIVITY_IMPORTS)
    activity.write_text(updated, encoding="utf-8")
    return f"injected the PiP lifecycle into {activity}"


def prepare(root: Path, gen: Path) -> list[str]:
    notes: list[str] = []
    lib_rs = root / "src-tauri" / "android-plugin" / "src" / "lib.rs"
    package, class_name = rust_registration(lib_rs)

    kt_src = kotlin_source(root, package, class_name)
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

    # The PiP controller: Kotlin half (template) + the shared WebView JS.
    pip_js = read_pip_controller_js(root)
    pip_template = kotlin_source(root, package, PIP_CONTROLLER_CLASS)
    if not pip_template.is_file():
        raise SystemExit(f"PiP controller template not found at {pip_template}")
    pip_pkg = package_of(pip_template)
    if pip_pkg != package:
        raise SystemExit(
            f"Kotlin package {pip_pkg!r} != register_android_plugin package {package!r} "
            f"({pip_template})"
        )
    pip_target = dest / f"{PIP_CONTROLLER_CLASS}.kt"
    pip_target.write_text(render_pip_controller(pip_template, pip_js), encoding="utf-8")
    notes.append(
        f"copied {PIP_CONTROLLER_CLASS}.kt -> {pip_target} "
        f"({len(pip_js)} bytes of pip_controller.js embedded)"
    )

    notes.append(ensure_pip_lifecycle(activity))
    dest_controller = dest / f"{PIP_CONTROLLER_CLASS}.kt"
    if not dest_controller.is_file():
        raise SystemExit(f"controller was not written to {dest_controller}")

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
        if "import java.util.Properties" not in text:
            text = "import java.util.Properties\n" + text
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
