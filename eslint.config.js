import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import tseslint from 'typescript-eslint'
import { defineConfig, globalIgnores } from 'eslint/config'

export default defineConfig([
  // `src-tauri/target` is the Rust build directory - tens of thousands of
  // files that ESLint has no business walking, and walking them made the
  // whole run fail on an unreadable entry rather than lint anything.
  globalIgnores(['dist', 'src-tauri/target', 'src-tauri/gen', 'coverage']),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
    ],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
      // Type-aware linting, for one rule. The type-check pass costs the same
      // whether one rule needs it or forty, so the cost is the entry fee, not
      // the rule count.
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    rules: {
      // A dropped promise fails silently: the feature behind it just never
      // happens, with nothing in the console and nothing failing. That is how
      // the tray's update listener came to be registered inside a chain with
      // no rejection handler. The rest of `recommendedTypeChecked` is not on
      // — 49 of its 73 findings here were `onClick={asyncFn}`, which is the
      // idiom rather than a defect.
      '@typescript-eslint/no-floating-promises': 'error',
    },
  },
])
