# Hardened GitHub Actions workflows

These are drop-in replacements for `.github/workflows/` with every external
Action pinned to a commit SHA and the Android job verifying the **resolved**
Gradle pins (not only the source text).

They are kept here, outside `.github/workflows/`, because the branch was pushed
with a token that has no `workflow` scope. Apply them on a machine/token that is
allowed to modify workflows:

```bash
./ci-fixes/apply.sh          # copies the four files into .github/workflows/
git add .github/workflows
git commit -m "ci: pin actions and verify resolved Android pins"
git push
```

`apply.sh` is idempotent and only copies these four files. Review the diff
before committing; no secret is needed by CI itself.
