import path from "node:path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// The React app is served by Vite in dev; `/api/*` proxies to the ac-ai-sdk
// Rust host. To the browser everything is same-origin (localhost:5173), so the
// host's Origin check passes and there's no CORS.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  server: {
    port: 5173,
    proxy: {
      // Workspace files (generated images etc.) — GET /api/files/<path>.
      "/api/files": {
        target: "http://127.0.0.1:8790",
        changeOrigin: false,
      },
      "/api": {
        target: "http://127.0.0.1:8790",
        changeOrigin: false,
      },
    },
  },
});
