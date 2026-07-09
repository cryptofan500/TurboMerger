/* ESLint 8 (eslintrc-style) config. This file was missing entirely — the
 * `npm run lint` script (and the CI lint step) had never actually run. */
module.exports = {
  root: true,
  env: { browser: true, es2021: true },
  parser: "@typescript-eslint/parser",
  plugins: ["@typescript-eslint", "react-hooks"],
  extends: [
    "eslint:recommended",
    "plugin:@typescript-eslint/recommended",
    "plugin:react-hooks/recommended",
  ],
  ignorePatterns: ["dist", "node_modules", "src-tauri"],
};
