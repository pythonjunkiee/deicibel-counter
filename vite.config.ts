import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [react()],

  // Prevent Vite from obscuring Rust compiler errors
  clearScreen: false,

  server: {
    // Tauri expects a fixed port; fail if it's already in use
    port: 1420,
    strictPort: true,
    watch: {
      // Don't watch the Rust source — cargo handles that
      ignored: ["**/src-tauri/**"],
    },
  },
});
