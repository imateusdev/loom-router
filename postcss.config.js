export default {
  plugins: {
    // Tailwind 4 moved the PostCSS integration to its own package, and folds
    // vendor prefixing in, so autoprefixer is gone from the pipeline.
    '@tailwindcss/postcss': {},
  },
}
