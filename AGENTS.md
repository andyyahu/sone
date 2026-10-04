# Repository Guidelines

## Project Structure & Module Organization

SONE is a Linux TIDAL client built with React, TypeScript, and Tauri 2/Rust.

- `src/components/` contains UI components; `src/hooks/` holds React hooks; `src/atoms/` manages Jotai state. Shared helpers live in `src/lib/` and `src/utils/`; `src/api/` provides the frontend Tauri bridge.
- `src-tauri/src/` implements native services, audio playback, and API access; `commands/` exposes Tauri commands. Rust integration tests live in `src-tauri/tests/`.
- `public/` contains static frontend assets; `src-tauri/icons/` holds application icons; `data/` contains screenshots and desktop metadata.
- `build-scripts/`, `nix/`, and `snap/` support distribution packaging.

## Build, Test, and Development Commands

Use Node.js 22+, pnpm 11.1.3 (pinned in `package.json`), and stable Rust. Install the Linux system dependencies listed under “Building from source” in `README.md`.

- `pnpm install --frozen-lockfile`: install frontend dependencies reproducibly.
- `pnpm tauri dev`: run the desktop app with the Vite development server.
- `pnpm dev`: run the frontend development server alone.
- `pnpm build`: type-check TypeScript and generate frontend assets in `dist/`.
- `pnpm tauri build`: build release desktop packages.
- `pnpm test`: run frontend tests once.
- `cargo test --manifest-path src-tauri/Cargo.toml --locked`: run Rust tests. Run `pnpm build` first because Tauri embeds `dist/`.
- `pnpm check`: run ESLint, Prettier checks, Clippy, rustfmt checks, and Knip. Build frontend assets first; this command does not run tests.

## Coding Style & Naming Conventions

Use strict TypeScript. Prettier specifies two-space indentation, double quotes, semicolons, trailing commas, and an 80-column print width. Run `pnpm format` for frontend formatting and `pnpm fmt:rust` for Rust formatting. Follow PascalCase component filenames, `useCamelCase` hook names, camelCase helpers, and snake_case Rust modules/functions. Keep Clippy warning-free.

## Testing Guidelines

Frontend tests use Vitest, jsdom, and React Testing Library. Colocate `*.test.ts` or `*.test.tsx` files with source; `*.spec.ts(x)` is also supported. Import Vitest helpers explicitly. Run a focused test with `pnpm test src/lib/theme.test.ts`. Rust uses unit/integration tests and proptest. Add regression coverage for behavior changes; no numeric coverage threshold is configured.

## Commit & Pull Request Guidelines

Follow the history’s Conventional Commit style, such as `fix(home): restore empty feed handling` or `chore(deps): update dependencies`. Use concise, imperative subjects. Describe the problem and resulting behavior in PRs, link related issues, include validation results, and attach screenshots for visible UI changes.
