// The only general process API for JS/TS tests. Bash fixtures use runBash instead.
import * as cp from "node:child_process";
import { promisify } from "node:util";
import { basename } from "node:path";

const shells = new Set(["sh", "bash", "rbash", "dash", "zsh", "ksh", "mksh", "csh", "tcsh", "ash", "fish", "pwsh", "powershell", "elvish", "cmd", "command"]);
export function processArguments(file, args = [], options = {}) {
  if (typeof file !== "string" || !Array.isArray(args) || args.some(arg => typeof arg !== "string")) throw new TypeError("process requires a file and string argument array");
  const command = basename(file.replaceAll("\\", "/")).toLowerCase().replace(/\.(exe|com|bat|cmd)$/, "");
  if (shells.has(command) || command === "env" || command === "npx" || command === "yarn" || command === "pnpm" || command === "busybox" || /\.(sh|bash|zsh|ksh|ps1|bat|cmd)$/i.test(file)) throw new Error("use runBash for shell fixtures; indirect command launchers are refused");
  // The browser stack's existing web build is the sole npm invocation. New script
  // entry points need explicit review here, including their package.json script.
  const browserBuild = args.length === 8 &&
    args.slice(0, 6).join("\0") === ["run", "build", "--workspace", "@omg/web", "--", "--outDir"].join("\0") &&
    args[7] === "--emptyOutDir";
  if (command === "npm" && !browserBuild) throw new Error("unsupported npm test entry point");
  if ((command === "node" || command === "nodejs") && (args[0]?.startsWith("-") || !/\.[cm]?js$/i.test(args[0] ?? ""))) throw new Error("Node test entry points require a JavaScript file");
  if ("shell" in options) throw new Error("shell options are forbidden; use runBash");
  return [file, [...args], { ...options, shell: false }];
}
export const spawnSync = (file, args, options) => cp.spawnSync(...processArguments(file, args, options));
export const spawn = (file, args, options) => cp.spawn(...processArguments(file, args, options));
export const execFileSync = (file, args, options) => cp.execFileSync(...processArguments(file, args, options));

export function execFile(file, args, options, callback) {
  if (typeof options === "function") [options, callback] = [{}, options];
  return cp.execFile(...processArguments(file, args, options), callback);
}
// Match Node's promisified execFile result ({ stdout, stderr }), used by browser helpers.
execFile[promisify.custom] = (file, args, options) => promisify(cp.execFile)(...processArguments(file, args, options));
