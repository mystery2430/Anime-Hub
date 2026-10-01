// Print a short GitHub Actions annotation from a build log.
// Log blobs from this runner are not always downloadable; the checks API
// returns annotations, and the step summary is on the run page.
// package.json sets "type": "module"; keep this file ESM.
import fs from "node:fs";
import { execFileSync } from "node:child_process";

const logPath = process.argv[2];
const label = process.argv[3] || "log";

let raw = "";
try {
  raw = fs.readFileSync(logPath, "utf8");
} catch (err) {
  raw = String(err);
}
// Cargo/clippy colour the diagnostics; the ANSI resets between `error` and
// `:` used to break every regex below. Strip escapes up front.
raw = raw.replace(/\x1b\[[0-9;]*[A-Za-z]/g, "");

// Gradle prints the actual cause between these two markers; the generic
// line filter misses it (e.g. "A problem occurred evaluating project ':app'"),
// so lift the whole block verbatim.
let gradleBlock = "";
const gm = raw.match(/FAILURE: Build failed[\s\S]{0,2200}?BUILD FAILED[^\n]*/);
if (gm) gradleBlock = gm[0].split(/\r?\n/).filter((l) => l.length < 300).join("\n");

// Rust/cargo error lines with a few context lines each (the caret snippet
// and the `--> location` line carry the actual diagnostic).
const lines = raw.split(/\r?\n/);
const context = [];
lines.forEach((line, i) => {
  if (/^(\x1b\[[0-9;]*m)*error(\[|:)/i.test(line) && !/could not compile/i.test(line)) {
    context.push(...lines.slice(i, i + 5));
  }
});
const contextBlock = [...new Set(context)].slice(0, 40).join("\n");

const interesting = lines.filter((line) =>
  /error(\[|:)|warning:|FAILED|panicked|could not compile|test result:|^\s*-->|What went wrong|Execution failed|Caused by|npm ERR!|Traceback|No such file|not found|^\s*>\s/i.test(
    line,
  ),
);
const picked = (interesting.length ? interesting : lines.slice(-12)).slice(-20);

// The last lines always go in: a tool can fail with a single message that
// matches none of the patterns above (the Android job printed only the npm
// banner once), and without them the annotation says nothing at all.
const tail = lines.filter((line) => line.trim() !== "").slice(-25);
const tailBlock = tail.join("\n").slice(0, 1600);

const parts = [];
if (gradleBlock) parts.push(gradleBlock);
if (contextBlock) parts.push(contextBlock);
// Skip the tail when it is already the tail of `picked` (avoids a duplicate).
if (tailBlock && !picked.join("\n").includes(tail[tail.length - 1])) {
  parts.push(tailBlock);
}
parts.push(picked.join("\n"));
const text = parts.join("\n--\n");
const finalText = text.slice(0, 4000);

const msg = finalText.replace(/%/g, "%25").replace(/\r/g, "%0D").replace(/\n/g, "%0A");
console.log(`::error::${label}:%0A${msg}`);

try {
  const summary = process.env.GITHUB_STEP_SUMMARY;
  if (summary) {
    fs.appendFileSync(summary, `\n### ${label}\n\n\`\`\`\n${finalText}\n\`\`\`\n`);
  }
} catch {
  // The annotation above is the part CI can read back.
}

// Job logs live on a host this sandbox cannot download. A pull-request
// comment is readable through the GitHub API. execFileSync so the process
// does not exit before the request finishes.
const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
const repo = process.env.GITHUB_REPOSITORY;
const pr = process.env.PR_NUMBER;
if (token && repo && pr) {
  const body = `**${label} failed**\n\n\`\`\`\n${text.slice(0, 1800)}\n\`\`\``;
  try {
    execFileSync(
      "gh",
      ["api", `repos/${repo}/issues/${pr}/comments`, "-f", `body=${body}`],
      { env: { ...process.env, GH_TOKEN: token }, stdio: "ignore" },
    );
  } catch {
    // Annotation already printed.
  }
}
