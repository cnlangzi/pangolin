/** @type {import('tailwindcss').Config} */
module.exports = {
  // TailwindCSS auto-scans templates for class usage (purgecss).
  content: [
    './crates/admin/templates/**/*.html',
    './assets/**/*.html',
  ],
  theme: {
    extend: {
      colors: {
        // Pangolin brand: deep slate sidebar + electric blue accent
        brand: {
          50:  '#eff6ff',
          100: '#dbeafe',
          200: '#bfdbfe',
          300: '#93c5fd',
          400: '#60a5fa',
          500: '#3b82f6', // primary blue
          600: '#2563eb',
          700: '#1d4ed8',
          800: '#1e40af',
          900: '#1e3a8a',
        },
        pangolin: {
          slate:   '#0f172a', // sidebar bg — very dark slate
          steel:   '#1e293b', // sidebar card bg
          zinc:    '#334155', // sidebar border
          silver:  '#94a3b8', // muted text
          white:   '#f8fafc', // sidebar text
          accent:  '#06b6d4', // cyan — tunnel/connection accent
          success: '#22c55e', // online
          warning: '#f59e0b', // expiring soon
          danger:  '#ef4444', // error/offline/delete
          info:    '#3b82f6', // link/active
        },
      },
      fontFamily: {
        sans: ['Inter', 'system-ui', '-apple-system', 'sans-serif'],
        mono: ['JetBrains Mono', 'Fira Code', 'monospace'],
      },
    },
  },
  plugins: [],
}