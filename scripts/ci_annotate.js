// Print a short GitHub Actions annotation from a build log.
// Log blobs from this runner are not always downloadable; the checks API
// returns annotations, and the step summary is on the run page.
// package.json sets "type": "module"; keep this file ESM.
import fs from "node:fs";

const logPath = process.argv[2];
const label = process.argv[3] || "log";

let raw = "";
try {
  raw = fs.readFileSync(logPath, "utf8");
} catch (err) {
  raw = String(err);
}

// Gradle prints the actual cause between these two markers; the generic
// line filter misses it (e.g. "A problem occurred evaluating project ':app'"),
// so lift the whole block verbatim.
let gradleBlock = "";
const gm = raw.match(/FAILURE: Build failed[\s\S]{0,2200}?BUILD FAILED[^\n]*/);
if (gm) gradleBlock = gm[0].split(/\r?\n/).filter((l) => l.length < 300).join("\n");

const interesting = raw.split(/\r?\n/).filter((line) =>
  /error(\[|:)|warning:|FAILED|panicked|could not compile|test result:|^\s*-->|What went wrong|Execution failed|Caused by|^\s*>\s/i.test(
    line,
  ),
);
const picked = (interesting.length ? interesting : raw.split(/\r?\n/).slice(-12)).slice(-20);
const text = (gradleBlock ? gradleBlock + "\n--\n" : "") + picked.join("\n");
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
const { execFileSync } = require("child_process");
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
