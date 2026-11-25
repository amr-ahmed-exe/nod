# Nod (Rust Edition)

**Nod** (`amr-ahmed-exe/nod`) is a fast, lightweight native Rust distribution of Nod with an Alpine Linux container runtime, System Tray background mode, automatic GitHub Releases updates, Multi-Bot isolation, Pages (Artifacts), Jobs, and headless Linux VPS support.

---

## Credits & Attribution

Nod (`amr-ahmed-exe/nod`) builds directly on the shoulders of upstream creators:

- **[`n8n-io/nod`](https://github.com/n8n-io/nod)** — Original creators and copyright holders under the [Apache License 2.0](LICENSE). The web frontend (`src/web/`), shared schemas (`src/shared/`), app catalog (`src/apps/`), container sandbox architecture, and documentation belong to the upstream Nod project.
- **[`mohamed0s/nod`](https://github.com/mohamed0s/nod)** — Fork inspiration for the **Pages (Artifacts)** and **Jobs** features.
- **[`amr-ahmed-exe/nod`](https://github.com/amr-ahmed-exe/nod)** — Native Rust (`nod-rs`) desktop & headless backend rewrite, Alpine Linux runtime conversion, low-RAM resource tuning, Windows System Tray & hide-to-tray lifecycle, SHA-256 self-updater, AMOLED & Catppuccin Mocha themes, and public GitHub Actions CI/Release pipelines.

Notice: Upstream TypeScript server code (`src/server/`) and Electron packaging (`desktop/main/`) have been removed from this fork in favor of `nod-rs/`, so upstream TypeScript PRs are not auto-synced into this repository. See [NOTICE](NOTICE) and [LICENSE](LICENSE).

---

## What Changed in This Fork

- **Native Rust Backend & Shell (`nod-rs/`)**: Single lightweight binary (`tao` + `wry` WebView2 on Windows, WebKitGTK on Linux, or `--headless` with zero GUI dependencies).
- **Alpine Linux Guest & Low-RAM Tuning**:
  - Windows WSL2 guest image (`desktop/runtime/Dockerfile.windows`) and workspace container (`Dockerfile.workspace`) run on **`alpine:3.21`** instead of Ubuntu 24.04.
  - Tuned container & database memory budgets (`256m` Postgres, `1g` default for Workspace and Browser via `NOD_WORKSPACE_MEMORY` / `NOD_BROWSER_MEMORY`, SQLite tuned pragmas) for modest laptops and low-RAM servers.
- **System Tray & Background Mode (Windows)**:
  - Clicking **Close (`X`)** hides the window to the notification area so scheduled tasks and bots continue running.
  - Left-click tray icon or click **Open Nod** to restore; use **Quit Nod** in the tray menu to stop everything cleanly.
- **Automatic Self-Updater**:
  - Checks `https://api.github.com/repos/amr-ahmed-exe/nod/releases/latest` automatically (and via the tray menu), verifies `SHA256SUMS.txt` / asset digest before staging, shows update status in the UI toast and tray balloon notification, and applies on restart.
- **Themes & Multi-Bot Isolation**:
  - Includes **Light**, **Dark**, **AMOLED**, and **Catppuccin Mocha** themes.
  - Main Bot can see secondary bots, secondary bots are isolated from each other and from Main Bot, and Group Chat supports `@mentions`.
- **Pages (Artifacts) & Jobs**:
  - Live interactive HTML artifact creation, versioning, full-screen preview, and Jobs tracking (`POST/GET/PATCH/DELETE` with schema validation).

---

## Running & Building

### Prerequisites

- **Rust** (stable toolchain)
- **Node.js 22+** & **pnpm 10+** (only needed to rebuild frontend assets)
- **Docker** (for container workspace/browser) or **WSL2** on Windows

### Build UI + Native Desktop App

```sh
pnpm install --frozen-lockfile
pnpm run build
cargo build --manifest-path nod-rs/Cargo.toml --release
```

Copy built UI assets (`dist/web` + `dist/desktop-ui` setup files) into `nod-rs/web/` before compiling `nod-rs` if developing locally.

### Run Headless (Linux VPS / Server / Docker)

Build without desktop GUI libraries (`tao` / `wry` / WebKitGTK):

```sh
cargo build --manifest-path nod-rs/Cargo.toml --release --no-default-features
./nod-rs/target/release/nod --headless --host 0.0.0.0 --port 3000
```

Or run directly with Docker Compose (`deployment/self-host/`):

```sh
cd deployment/self-host
sh setup.sh
docker compose --file compose.yaml --env-file .env up -d --build --wait
```

---

## Automated Releases (GitHub Actions)

- **CI (`.github/workflows/ci.yml`)**: Type-checks and builds the frontend UI, runs `cargo clippy --all-targets -- -D clippy::all -D warnings`, and runs the Rust test suite on Windows (`windows-latest`) and Linux (`ubuntu-24.04`), including `--no-default-features` headless compilation.
- **Release (`.github/workflows/release.yml`)**: Triggered by tags matching `v*` or `nod-v*` (or `workflow_dispatch`):
  - `nod-windows-x86_64.exe` & `nod-windows-x86_64.zip`
  - `nod-linux-x86_64` & `nod-linux-x86_64.tar.gz`
  - `nod-linux-x86_64-headless` & `nod-linux-x86_64-headless.tar.gz`
  - `nod-Runtime-win32-x64.tar.gz` + `manifest.json` (Alpine Linux WSL2 guest image)
  - `SHA256SUMS.txt` (used by the built-in self-updater)

Override update checks or disable auto-updates with environment variables:

- `NOD_UPDATE_REPO=owner/repo`
- `NOD_DISABLE_AUTO_UPDATE=1`
- `NOD_HEADLESS=1`
