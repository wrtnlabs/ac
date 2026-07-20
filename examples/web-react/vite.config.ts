import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The React app is served by Vite in dev; `/api/*` proxies to the ac-ai-sdk
// Rust host. To the browser everything is same-origin (localhost:5173), so the
// host's Origin check passes and there's no CORS.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8790",
        changeOrigin: false,
      },
    },
  },
});
