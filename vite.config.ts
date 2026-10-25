import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed dev port and does not want Vite to clear its output.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**", "**/engine/**", "**/analyzer/src/**", "**/stems/**"] },
  },
  build: {
    target: "safari15",
    sourcemap: false,
    // Two windows: the main window and the Analyzer.
    rollupOptions: { input: { main: "index.html", analyzer: "analyzer.html" } },
  },
});
