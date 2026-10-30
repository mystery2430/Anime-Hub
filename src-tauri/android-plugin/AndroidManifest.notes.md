# Android manifest additions for AnimeHub

`npm run tauri android init` generates
`src-tauri/gen/android/app/src/main/AndroidManifest.xml`. Tauri's default
activity does **not** declare Picture-in-Picture support, so add these
attributes to the generated `<activity>` element:

```xml
<activity
    android:configChanges="orientation|keyboardHidden|keyboard|screenSize|layoutDirection|locale|screenLayout|uiMode|smallestScreenSize|density|navigation"
    android:supportsPictureInPicture="true"
    ... >
```

Two things matter:

1. **`android:supportsPictureInPicture="true"`** — without it,
   `enterPictureInPictureMode()` throws `IllegalStateException`.
2. **`android:configChanges`** must include everything the activity handles
   itself, otherwise entering PiP triggers a configuration change that
   recreates the activity and kills playback. The list above is Tauri's
   default plus `uiMode`/`screenLayout`; keep whatever Tauri generated and
   only add what is missing.

## OAuth deep link

The AniList redirect uses the custom scheme `animehub://anilist/callback`.
`scripts/android_prepare.py` adds this filter to the same `<activity>` when the
generated manifest does not have it yet:

```xml
<intent-filter>
    <action android:name="android.intent.action.VIEW" />
    <category android:name="android.intent.category.DEFAULT" />
    <category android:name="android.intent.category.BROWSABLE" />
    <data android:scheme="animehub" android:host="anilist" android:pathPrefix="/callback" />
</intent-filter>
```

The scheme matches `OAUTH_SCHEME` in `src-tauri/src/lib.rs`. It is **not** the
Android OAuth redirect: native AniList login on Android is loopback-only
(`anilist::mobile_native_login_allowed`), a custom-scheme redirect is refused
before any pending state or authorization URL exists, and
`plugins.deep-link.mobile` is `[]`. Nothing in the app sends
`animehub://anilist/callback` on Android, so the filter is inert: an unsolicited
intent is parsed against the configured loopback redirect and rejected. It is
still declared (and pinned by `tests/gaps.test.js`) because removing it is a
manifest change that needs an Android build and a device check. `DESKTOP` uses
the scheme for real, registered at startup from
`plugins.deep-link.desktop.schemes`.

Tauri's deep-link plugin builds its own mobile filter from
`plugins.deep-link.mobile`. That field is a list of domains, not a map — a map
fails the plugin build script with `invalid type: map, expected a sequence`. It
stays empty here.

## Keystore bridge

Do not copy the Kotlin file by hand. After `tauri android init`, run
`python3 scripts/android_prepare.py`. It copies
[`kotlin/dev/animehub/app/AnimeHubPlugin.kt`](kotlin/dev/animehub/app/AnimeHubPlugin.kt)
next to the generated `MainActivity.kt`, adds
`android:supportsPictureInPicture="true"` if it is missing, adds the
`animehub://anilist/callback` intent filter if Tauri did not, and writes a
ProGuard keep rule so R8 cannot strip a class that is only named from JNI.

The script exits non-zero if the generated package is not `dev.animehub.app`.
No manifest permission is needed for the keystore: `AndroidKeyStore` is
available to every app without a runtime permission.

## Verification status

The manifest snippets above use stable Android attributes and the Tauri 2
generated project layout, but **they have not been built or run here** — that
requires the Android SDK + NDK, which were not available in the environment
this project was written in. Confirm with `npm run tauri android dev` on a
real device before shipping.
