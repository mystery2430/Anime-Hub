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
Add an intent filter to the same `<activity>`:

```xml
<intent-filter>
    <action android:name="android.intent.action.VIEW" />
    <category android:name="android.intent.category.DEFAULT" />
    <category android:name="android.intent.category.BROWSABLE" />
    <data android:scheme="animehub" android:host="anilist" android:pathPrefix="/callback" />
</intent-filter>
```

The scheme must match `OAUTH_SCHEME` in `src-tauri/src/lib.rs` and the
`plugins.deep-link` block in `src-tauri/tauri.conf.json`. The redirect URI
registered with AniList must be exactly `animehub://anilist/callback`.

Tauri's deep-link plugin also generates this filter from the
`plugins.deep-link.mobile` config on `tauri android init`, so check the
generated manifest first and only add it by hand if it is absent.

## Keystore bridge

Copy
[`dev_animehub_app/AnimeHubPlugin.kt`](dev_animehub_app/AnimeHubPlugin.kt)
into `src-tauri/gen/android/app/src/main/java/dev_animehub_app/`, next to the
generated `MainActivity.kt`. No manifest entry or permission is needed:
`AndroidKeyStore` is available to every app without a runtime permission.

## Verification status

The manifest snippets above use stable Android attributes and the Tauri 2
generated project layout, but **they have not been built or run here** — that
requires the Android SDK + NDK, which were not available in the environment
this project was written in. Confirm with `npm run tauri android dev` on a
real device before shipping.
