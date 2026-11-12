# Android crash and ANR diagnostics

Status: a procedure, not a verified result. Nothing here was run on a device
from the sandbox. Use it when the app closes or freezes on a phone. For
feature-flow and diagnostics-emulator testing, see
[`android-emulator-diagnostics.md`](./android-emulator-diagnostics.md).

## What the in-app log is and is not

- The in-app diagnostics timeline (`AnimeHubDebugLog`, Settings → developer
  options) merges the bounded Rust and Kotlin rings: up to 2,000 events per
  source (4,000 combined), with sequence/time ordering, dropped-event count and
  opaque operation correlation. It is memory-only and is lost when the process
  dies. It does not contain URLs, hosts, profile names, cookies or page data.
  Copy/export is user-triggered and does not send the file anywhere.
- It is **not** a crash dump. It cannot show a native crash, an ANR, or the
  reason the system ended the process. Those come from `logcat` and `dumpsys`.

## Prerequisites

- USB debugging on the phone, and `adb` on the computer.
- Reproduce the problem once, then collect logs right away, before the buffer
  rotates.

## Steps

Run these in a terminal. Replace `<serial>` with the device serial from `adb devices`.

1. **Log buffer dump (includes the crash stack trace):**
   ```
   adb -s <serial> logcat -d -v threadtime > animehub-logcat.txt
   ```
   Search the file for `FATAL EXCEPTION`, `AndroidRuntime`, `ANR in`,
   `AnimeHubPlugin`, `AnimeHubPip`, `AnimeHubNavGuard`.

2. **Why the system ended the process (Android 11 and later):**
   ```
   adb -s <serial> shell dumpsys activity exit-info dev.animehub.app
   ```
   Look at the `description` and `reason` fields. `ANR` means the UI thread
   was blocked; `crash` means an uncaught exception; `native crash` means a
   native fault.

3. **Process state and recent activity:**
   ```
   adb -s <serial> shell dumpsys activity processes dev.animehub.app
   ```

4. **ANR traces (may be unavailable without root on many devices):**
   ```
   adb -s <serial> pull /data/anr/traces.txt animehub-anr-traces.txt
   ```
   If the file is missing or permission is denied, note that and rely on step 2.
   Do not try to work around the permission.

5. **Native crash tombstones (may be unavailable without root):**
   ```
   adb -s <serial> logcat -b crash -d
   ```

6. **Full bug report (last resort; large and personal):**
   ```
   adb -s <serial> bugreport animehub-bugreport.zip
   ```
   The zip holds device-wide data. Do not share it publicly.

## Redaction rules before sharing any file

Before sharing *any* in-app export, logcat excerpt, screenshot or bug-report
snippet, verify that none of these values appear; delete or replace the whole
value rather than keeping a recognizable prefix:

- Cookies, `Cookie:` / `Set-Cookie:` headers, session tokens, passwords,
  `Authorization` headers, API keys, or any string that looks like a token.
- The AniList client secret or any value stored under the AniList settings.
- Full or partial URLs, hostnames, origins, geolocation origins, query strings,
  page/storage content, site IDs, profile names or filesystem paths.
- Device serials, account names, email addresses and other personal data.
- Anything from a bug report beyond the specific lines needed for the issue.

The in-app export is schema-limited and should already contain only safe event
codes and counts. Still inspect it before sharing, especially if it came from an
older APK. A redaction check is a required sharing step, not a claim that
logcat or a system bug report is safe by default.

Never paste a log that still contains a cookie value or a secret. Rotate the
secret if one was pasted by mistake.

## What to report

- App version (from the About screen or the build), Android version, device model.
- Which step you were on when the problem happened.
- The `exit-info` description line and the matching `FATAL EXCEPTION` or
  `ANR in` lines from step 1, redacted.
- Whether the problem repeats after a cold start.

## Known limits

- The `exit-info` output and the logcat buffer are device-dependent and
  rotate; collect them soon after the event.
- Without root, ANR traces and tombstones may be unavailable. This procedure
  does not claim they are.
- This procedure does not prove the cause of any particular crash. It only
  collects the evidence needed to decide.
