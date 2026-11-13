import react from "@vitejs/plugin-react";
import { loadEnv } from "vite";
import { defineConfig } from "vitest/config";
import { mockApi } from "./src/mocks/vite-plugin.ts";

// `pnpm dev` proxies the API to the server at REWIND_URL; `pnpm dev:mock` answers it with the
// mocked API of `src/mocks`. The build has relative paths only: the server can serve it under
// any prefix, and routing lives in the URL fragment, so it needs no fallback route.
export default defineConfig(({ command, mode }) => {
  const env = loadEnv(mode, process.cwd(), "REWIND_");
  const serving = command === "serve" && mode !== "test";
  if (serving && mode !== "mock" && !env.REWIND_URL) {
    throw new Error(
      "Set REWIND_URL to the URL of a rewind server (REWIND_URL=http://localhost:8080 pnpm dev), or run pnpm dev:mock.",
    );
  }
  return {
    base: "./",
    plugins: [react(), mode === "mock" ? mockApi() : null],
    build: {
      outDir: "dist",
      assetsDir: "assets",
      target: "es2023",
      sourcemap: false,
      chunkSizeWarningLimit: 800,
    },
    server: {
      proxy:
        serving && mode !== "mock"
          ? { "/v1": { target: env.REWIND_URL, changeOrigin: true } }
          : undefined,
    },
    test: {
      environment: "jsdom",
      setupFiles: ["./src/test/setup.ts"],
      include: ["src/**/*.test.{ts,tsx}"],
      restoreMocks: true,
    },
  };
});
