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
6. After merge, main CI must pass. `publish-release.yml` then calls both release
   workflows to rebuild that exact main CI SHA (not a moving branch). Both fresh
   production build matrices must succeed before its publish job can run.
7. The publish job downloads only production artifacts from its own run, checks
   an exact seven-installer allowlist, and generates `SHA256SUMS`. It creates a
   draft at the verified SHA, checks the eight assets, then publishes latest,
   non-prerelease. The CI debug APK cannot pass the allowlist. Existing v0.3.3
   tags are skipped, never overwritten. This pipeline is scoped to v0.3.3.
8. Verify public metadata, assets and tag target; report CI/build URLs and risks.
   Preserve v0.3.2. If draft upload fails, inspect manually; do not overwrite tags.

The Actions-based publisher avoids downloading artifacts through this sandbox,
which cannot reach GitHub's external artifact storage. Workflow dispatch also
requires Actions write permission; the initial branch builds can be started by
a maintainer. Publication runs only on successful main CI, never on PR code.

## Required release disclosures

No physical Android test was performed for v0.3.3. Compile/test success does not
prove PiP, IPC, navigation, persistence or performance on a device. Android
provider-to-provider same-origin localStorage/IndexedDB isolation is **not
guaranteed**; best-effort transfer is not a security boundary or lossless backup.
Android navigation lacks blocklist/DNS-rebinding/subframe/app-load coverage.
Windows packages remain unsigned without signing credentials. Real Linux Secret
Service behavior still needs manual validation. Cargo advisory warnings, if any,
must be reported separately from a successful audit exit status.
