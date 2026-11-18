// Git must read the selected checkout, even when tests run from a Git hook.
import { execFileSync } from "node:child_process";

export const gitEnv = () => ({
  ...Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("GIT_"))),
  GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1",
  GIT_AUTHOR_NAME: "fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid", GIT_COMMITTER_NAME: "fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid",
});

// NUL delimiters preserve literal paths; an invocation failure must fail the check.
export function gitFiles(cwd, { others = false } = {}) {
  const args = ["ls-files", "-z", "--cached", ...(others ? ["--others", "--exclude-standard"] : [])];
  const out = execFileSync("git", args, { cwd, encoding: "utf8", env: gitEnv(), stdio: "pipe", maxBuffer: 64 * 1024 * 1024 });
  return [...new Set(out.split("\0").filter(Boolean))].sort();
}
