import { defineConfig } from "vite";

// Tauri 期望固定端口，且失败时不自动换端口
export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "safari14",
    outDir: "dist",
  },
});
