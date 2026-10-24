import { defineConfig } from "vite";
import solid from "vite-plugin-solid";
import tailwindcss from "@tailwindcss/vite";

// The build goes straight into the supervisor, which embeds it.
export default defineConfig({
  plugins: [solid(), tailwindcss()],
  build: {
    outDir: "../../supervisor/ui-dist",
    emptyOutDir: true,
    target: "es2021",
    assetsInlineLimit: 0,
    reportCompressedSize: true,
  },
  server: {
    proxy: {
      "/api": { target: "http://127.0.0.1:7700", changeOrigin: false },
      "/auth": "http://127.0.0.1:7700",
    },
  },
});
