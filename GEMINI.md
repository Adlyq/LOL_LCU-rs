# LOL LCU Automation Tool (Rust)

A high-performance, asynchronous Windows-native automation tool for the League of Legends Client (LCU), written in Rust.

## Project Overview

- **Purpose:** Automates repetitive tasks in League of Legends such as auto-accepting matches, auto-skipping honor votes, and monitoring client memory usage to trigger reloads when necessary.
- **Core Technologies:**
  - **Runtime:** `tokio` (asynchronous I/O and task management).
  - **LCU Interaction:** `reqwest` (REST API) and `tokio-tungstenite` (WebSocket).
  - **GUI/Overlay:** `eframe` (egui) for the info panel and raw Win32 APIs for the in-game overlay.
  - **System Integration:** `windows` crate for deep Windows API access (window management, process scanning).
  - **Diagnostics:** `tracing` for structured logging (outputs to console and `lol_lcu.log`).

## Architecture

The project follows a modular design:
- `src/main.rs`: Application entry point, single-instance enforcement, and the main event loop.
- `src/app/`: Core business logic.
  - `handlers.rs`: Processes LCU events (gameflow, ready-check, champ-select).
  - `config.rs`: Persistent user configuration (saved in `%APPDATA%\lol-lcu\config.json`).
  - `state.rs`: In-memory session state.
- `src/lcu/`: LCU client implementation.
  - `api.rs`: REST API wrapper.
  - `connection.rs`: Handles LCU process detection and credential extraction.
  - `websocket.rs`: WebSocket event listener and broadcaster.
- `src/win/`: Windows-specific UI components.
  - `overlay.rs`: Transparent Win32 overlay for in-game information.
  - `info_panel.rs`: Main control panel UI built with `egui`.
  - `winapi.rs`: Low-level Windows API helpers.

## Building and Running

### Environment Requirements
- **OS:** Windows 10/11 (Required for Win32 API and Overlay).
- **Toolchain:** `x86_64-pc-windows-msvc`.

### Development
- **Standard (Windows CMD/PS):**
  - `cargo run`
- **WSL2 (Interoperability):**
  - Use the Windows-side cargo directly: `cargo.exe run`
  - *Note: Do not use Linux `cargo` as the project is Windows-specific.*

### Release
- **Build optimized binary:** `cargo build --release` (or `cargo.exe build --release`)
  - The release build uses `#![windows_subsystem = "windows"]` to avoid showing a console window.
- **Artifact:** `target/release/lol_lcu.exe`

## Development Conventions

- **Error Handling:** Uses `anyhow::Result` for application-level errors and `thiserror` for library-level errors.
- **Shared State:** Application configuration (`AppConfig`) and session state (`AppState`) are wrapped in `Arc<Mutex<T>>` (using `parking_lot` or `tokio::sync::Mutex`) for thread-safe access across the main loop and UI threads.
- **Logging:** Use the `tracing` macros (`info!`, `warn!`, `error!`, `debug!`).
- **Lints:** Adheres to standard Rust clippy lints.

## Features & Configurable Options

Settings can be modified via the Info Panel UI or manually in `config.json`:
- `auto_accept_enabled`: Automatically accept match-found prompts.
- `auto_accept_delay_secs`: Delay before auto-accepting (0-15s).
- `auto_honor_skip`: Automatically skip the honor ballot phase.
- `memory_monitor`: Automatically reload `LeagueClientUx.exe` if it exceeds a memory threshold.
- `memory_threshold_mb`: Threshold for memory-based reload (default: 1500MB).
- `premade_champ_select`: Analyze and display premade groups during champion select.
