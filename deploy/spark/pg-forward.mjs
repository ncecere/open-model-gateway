// Loopback-only TCP forwarder: the demo gateway and `bootstrap-demo` insist on a
// loopback database host, so this pod-local process relays 127.0.0.1:5432 to the
// CloudNativePG read-write Service. Test environments only; no logging of payloads.
import net from "node:net";
const target = process.env.PG_FORWARD_TARGET ?? "omg-db-rw";
const server = net.createServer(client => {
  const upstream = net.connect(5432, target);
  client.pipe(upstream).pipe(client);
  for (const socket of [client, upstream]) socket.on("error", () => { client.destroy(); upstream.destroy(); });
  for (const socket of [client, upstream]) socket.on("close", () => { client.destroy(); upstream.destroy(); });
});
server.listen(5432, "127.0.0.1");
for (const signal of ["SIGTERM", "SIGINT"]) process.on(signal, () => server.close(() => process.exit(0)));
