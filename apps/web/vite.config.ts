import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";
import { defineConfig, loadEnv } from "vite";

export default defineConfig(({ mode }) => {
  // This value is used by the development server only, never injected into the bundle.
  const env = loadEnv(mode, fileURLToPath(new URL(".", import.meta.url)), "GATEWAY_");
  const target = process.env.GATEWAY_INTERNAL_URL ?? env.GATEWAY_INTERNAL_URL ?? "http://127.0.0.1:8080";
  const proxy = Object.fromEntries(
    ["^/api(?:/|$)", "^/v1(?:/|$)", "^/health(?:/|$)"].map((path) => [path, { target }]),
  );

  return {
    plugins: [react()],
    resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
    server: { host: "127.0.0.1", port: 3000, strictPort: true, proxy },
    build: { outDir: "dist" },
  };
});
