import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The production base is supplied by the Rust asset route. Keeping it
// relative also makes preview builds work behind SELU__BASE_PATH.
export default defineConfig({
  base: './',
  plugins: [react()],
})
