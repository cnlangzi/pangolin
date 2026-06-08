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
        // Pangolin VI: 极简黑白 + 琥珀橙强调色
        accent: {
          50:  '#FFFBEB',
          100: '#FEF3C7',
          200: '#FDE68A',
          300: '#FCD34D',
          400: '#FBBF24',
          500: '#F59E0B', // 主强调色 - 琥珀橙
          600: '#D97706',
          700: '#B45309',
          800: '#92400E',
          900: '#78350F',
        },
        // 功能色
        success: '#10B981',
        warning: '#F59E0B',
        danger:  '#EF4444',
        info:    '#F59E0B',
      },
      fontFamily: {
        sans: [
          'system-ui',
          '-apple-system',
          'BlinkMacSystemFont',
          '"Segoe UI"',
          'Roboto',
          '"Helvetica Neue"',
          'Arial',
          '"Noto Sans"',
          'sans-serif',
          '"Apple Color Emoji"',
          '"Segoe UI Emoji"',
          '"Segoe UI Symbol"',
          '"Noto Color Emoji"',
        ],
        mono: [
          'ui-monospace',
          'SFMono-Regular',
          '"SF Mono"',
          'Monaco',
          'Menlo',
          'Consolas',
          '"Liberation Mono"',
          '"Courier New"',
          'monospace',
        ],
      },
    },
  },
  plugins: [],
  darkMode: 'media', // 支持系统深色模式
}