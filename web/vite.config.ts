import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Development API started with `cargo run -p audiobookai-service --example dev_server`.
const devApi = "http://127.0.0.1:8484";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    strictPort: true,
    proxy: {
      "/api": {
        target: devApi,
        changeOrigin: true,
        ws: true,
        // The service accepts mutations only from its own origin. Present proxied browser
        // requests as same-origin so the dev server can exercise the real CSRF/origin checks.
        configure: (proxy) => {
          proxy.on("proxyReq", (request) => {
            if (request.getHeader("origin")) request.setHeader("origin", devApi);
          });
          proxy.on("proxyReqWs", (request) => {
            if (request.getHeader("origin")) request.setHeader("origin", devApi);
          });
        },
      },
    },
  },
  build: {
    target: ["es2022", "chrome105", "safari13"],
    sourcemap: true,
  },
});
