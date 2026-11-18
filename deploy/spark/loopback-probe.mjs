// Native-sidecar startup checks run in the pod network namespace, where these
// listeners are loopback-only. No shell, credentials or non-loopback destinations.
import net from "node:net";
import { realpathSync } from "node:fs";
import { fileURLToPath } from "node:url";

export function probeLoopback(port) {
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error("invalid loopback port");
  return new Promise(resolve => {
    const socket = net.connect(port, "127.0.0.1");
    const finish = ready => { socket.destroy(); resolve(ready); };
    socket.once("connect", () => finish(true));
    socket.once("error", () => finish(false));
    socket.setTimeout(2000, () => finish(false));
  });
}

// ConfigMap mounts use symlinks; compare the real entry path with Node's module path.
if (process.argv[1] && realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const port = process.argv[2];
  if (process.argv.length !== 3 || !/^[1-9][0-9]{0,4}$/.test(port ?? "") || Number(port) > 65535) {
    process.exitCode = 1;
  } else {
    process.exitCode = await probeLoopback(Number(port)) ? 0 : 1;
  }
}
