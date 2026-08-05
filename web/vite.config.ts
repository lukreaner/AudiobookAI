import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    strictPort: true,
    proxy: {
      "/api": "http://127.0.0.1:8484",
    },
  },
  build: {
    target: ["es2022", "chrome105", "safari13"],
    sourcemap: true,
  },
});
