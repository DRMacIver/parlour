// Builds the islands into dist/, which the server embeds and serves at
// /static/islands/ (see server/src/islands.rs). The manifest tells the
// server which hashed files to load.
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  base: "/static/islands/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    manifest: true,
    target: "es2022",
    rollupOptions: {
      input: "src/main.tsx",
    },
  },
  test: {
    environment: "jsdom",
  },
});
