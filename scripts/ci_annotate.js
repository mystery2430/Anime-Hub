// Print a short GitHub Actions annotation from a build log.
// Log blobs from this runner are not always downloadable; the checks API
// returns annotations, and the step summary is on the run page.
const fs = require("fs");

const logPath = process.argv[2];
const label = process.argv[3] || "log";

let raw = "";
try {
  raw = fs.readFileSync(logPath, "utf8");
} catch (err) {
  raw = String(err);
}

const interesting = raw.split(/\r?\n/).filter((line) =>
  /error(\[|:)|warning:|FAILED|panicked|could not compile|test result:/i.test(line),
);
const picked = (interesting.length ? interesting : raw.split(/\r?\n/).slice(-12)).slice(-12);
const text = picked.join("\n").slice(0, 900);

const summary = process.env.GITHUB_STEP_SUMMARY;
if (summary) {
  fs.appendFileSync(summary, `\n### ${label}\n\n\`\`\`\n${text}\n\`\`\`\n`);
}

const msg = text.replace(/%/g, "%25").replace(/\r/g, "%0D").replace(/\n/g, "%0A");
console.log(`::error::${label}:%0A${msg}`);
