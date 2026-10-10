# Production release procedure (v0.3.3)

1. Update npm/Tauri/Rust versions and both lockfiles together. Preserve old tags.
2. Run `npm ci`, `npm test`, `npm run build`, `npm audit` locally.
3. Open the session-branch PR against main. Require all CI jobs: three-platform
   tests/fmt/clippy, Android aarch64 debug compile, release-profile LTO, dependency
   and secret audit. Audits/tests must not use continue-on-error.
4. Dispatch Release Desktop and Release Android (`sign=true`) on the exact
   session-branch commit. Both must succeed. These workflows do not publish tags
   or releases; they stage production artifacts. Android requires the keystore
   and verifies APK signatures. Desktop verifies Authenticode if configured.
5. Merge the explicit PR only after all checks succeed. Confirm the merge tree
   equals the tested tree; if different, rebuild and revalidate before publication.
6. Download only `animehub-desktop-linux`, `animehub-desktop-windows`, and the
   three `animehub-android-{aarch64,armv7,x86_64}` artifacts from those successful
   release runs. Never download CI's debug artifact for publication. Check seven
   package names, versions and nonempty sizes; record SHA-256 checksums.
7. Create a draft v0.3.3 release targeting the verified merge commit, upload the
   seven installers plus SHA256SUMS, then publish as latest, non-prerelease.
   Use GitHub CLI to create the new tag; do not push tags from the session branch.
8. Verify public release metadata, assets and tag target. Report PR/run URLs,
   checksum manifest and remaining risks. Do not modify v0.3.2.

## Required release disclosures

No physical Android test was performed for v0.3.3. Compile/test success does not
prove PiP, IPC, navigation, persistence or performance on a device. Android
provider-to-provider same-origin localStorage/IndexedDB isolation is **not
guaranteed**; best-effort transfer is not a security boundary or lossless backup.
Android navigation lacks blocklist/DNS-rebinding/subframe/app-load coverage.
Windows packages remain unsigned without signing credentials. Real Linux Secret
Service behavior still needs manual validation. Cargo advisory warnings, if any,
must be reported separately from a successful audit exit status.
