import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Build straight into the rust-embed folder consumed by the binary.
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "../ui",
    emptyOutDir: true,
  },
  server: {
    proxy: {
      "/api": "http://127.0.0.1:8410",
      "/v1": "http://127.0.0.1:8410",
    },
  },
});
