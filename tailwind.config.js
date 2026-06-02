/** @type {import('tailwindcss').Config} */
module.exports = {
  // Dynamic on-demand: scan admin templates for actual class usage.
  // Askama templates are .html files in crates/admin/templates/.
  content: [
    './crates/admin/templates/**/*.html',
    './crates/admin/src/**/*.rs',
    './assets/**/*.html',
  ],
  theme: {
    extend: {},
  },
  plugins: [],
}
