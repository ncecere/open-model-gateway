// Git must read the selected checkout, even when tests run from a Git hook.
import { execFileSync } from "node:child_process";

export const gitEnv = () => ({
  ...Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("GIT_"))),
  GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1",
  GIT_AUTHOR_NAME: "fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid", GIT_COMMITTER_NAME: "fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid",
});

// NUL delimiters preserve literal paths; an invocation failure must fail the check.
export function gitFiles(cwd, { others = false } = {}) {
  const args = ["ls-files", "-z", "--cached", ...(others ? ["--others", "--exclude-standard", "-t", "--stage"] : [])];
  const out = execFileSync("git", args, { cwd, env: gitEnv(), stdio: "pipe", maxBuffer: 64 * 1024 * 1024 });
  // A filename is byte data. Refuse undecodable paths before deduplication or filesystem access.
  const records = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(out).split("\0").filter(Boolean);
  if (!others) return [...new Set(records)].sort();
  const paths = records.map(record => {
    // With -t --stage an untracked record starts with "? "; index records
    // carry the mode, object ID and stage before the first tab.
    const untracked = record.startsWith("? ");
    const path = untracked ? record.slice(2) : record.slice(record.indexOf("\t") + 1);
    if (untracked ? path.endsWith("/") : record.slice(2, 8) === "160000") {
      throw new Error(`cannot inventory nested Git repository: ${path}`);
    }
    return path;
  });
  return [...new Set(paths)].sort();
}
