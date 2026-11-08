import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import type { Plugin } from "vite";
import { defineConfig } from "vitest/config";
import { mockApi } from "./mock/plugin.ts";

/** Ship the licence texts of the code and font that end up in dist/ (see THIRD_PARTY.md). */
function bundledLicences(): Plugin {
  const files: Record<string, string> = {
    "licenses/svelte-MIT.txt": "node_modules/svelte/LICENSE.md",
    "licenses/instrument-sans-OFL-1.1.txt": "node_modules/@fontsource/instrument-sans/LICENSE",
  };
  return {
    name: "nestegg-bundled-licences",
    apply: "build",
    generateBundle() {
      for (const [fileName, from] of Object.entries(files)) {
        this.emitFile({ type: "asset", fileName, source: readFileSync(new URL(from, import.meta.url)) });
      }
    },
  };
}

// Port of the dev server. Set NESTEGG_UI_PORT instead of passing --port, so the
// HMR websocket uses the same port (see below).
const port = Number(process.env.NESTEGG_UI_PORT ?? 5173);

export default defineConfig(({ command }) => ({
  // Relative asset URLs: the UI is served from whatever origin `nestegg ui` uses.
  base: "./",
  plugins: [svelte(), bundledLicences(), command === "serve" ? mockApi() : null],
  resolve: {
    alias: {
      "@fonts": fileURLToPath(new URL("./node_modules/@fontsource/instrument-sans/files", import.meta.url)),
    },
  },
  server: {
    host: "127.0.0.1",
    port,
    strictPort: true,
    // Behind `nestegg ui --dev-proxy` the page comes from nestegg's port. Its
    // login handler treats any `?token=` query as a login attempt, which also
    // catches Vite's websocket URL; so the HMR client talks to Vite directly.
    hmr: { host: "127.0.0.1", clientPort: port },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
    reportCompressedSize: true,
  },
  test: {
    include: ["tests/**/*.test.ts"],
    environment: "node",
  },
}));
