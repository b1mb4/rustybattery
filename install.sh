#!/usr/bin/env bash
# 🦀 RustyBattery — One-Command Universal Linux Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/b1mb4/rustybattery/main/install.sh | bash

set -euo pipefail

REPO="b1mb4/rustybattery"
INSTALL_DIR="${HOME}/.local/bin"
ARCH="$(uname -m)"

printf "\033[1;36m🦀 Installing RustyBattery — Per-App Power Tracker for Linux...\033[0m\n"

mkdir -p "$INSTALL_DIR"

ensure_sqlite_dev() {
    if ldconfig -p 2>/dev/null | grep -q "libsqlite3.so"; then
        return 0
    fi
    printf "\033[1;33mChecking SQLite3 shared library...\033[0m\n"
    if command -v dnf >/dev/null 2>&1; then
        sudo dnf install -y sqlite-devel
    elif command -v apt-get >/dev/null 2>&1; then
        sudo apt-get update && sudo apt-get install -y libsqlite3-dev
    elif command -v pacman >/dev/null 2>&1; then
        sudo pacman -Sy --noconfirm sqlite
    elif command -v zypper >/dev/null 2>&1; then
        sudo zypper install -y sqlite3-devel
    elif command -v apk >/dev/null 2>&1; then
        sudo apk add sqlite-dev
    fi
}

DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/rustybattery-linux-${ARCH}.tar.gz"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

INSTALLED_PREBUILT=0
if [ "${1:-}" != "--from-source" ] && command -v curl >/dev/null 2>&1; then
    printf "Checking for pre-built binary (%s)...\n" "$ARCH"
    if curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/rustybattery.tar.gz" 2>/dev/null; then
        tar -xzf "$TMP_DIR/rustybattery.tar.gz" -C "$TMP_DIR"
        if [ -f "$TMP_DIR/rustybattery" ]; then
            chmod +x "$TMP_DIR/rustybattery"
            rm -f "$INSTALL_DIR/rustybattery"
            cp "$TMP_DIR/rustybattery" "$INSTALL_DIR/rustybattery"
            INSTALLED_PREBUILT=1
            printf "\033[1;32m✔ Downloaded pre-built release binary.\033[0m\n"
        fi
    fi
fi

if [ "$INSTALLED_PREBUILT" -eq 0 ]; then
    printf "Building RustyBattery from source with Cargo...\n"
    ensure_sqlite_dev
    if ! command -v cargo >/dev/null 2>&1; then
        printf "\033[1;33mRust/Cargo not found. Installing via rustup...\033[0m\n"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        # shellcheck disable=SC1091
        source "$HOME/.cargo/env"
    fi
    git clone --depth 1 "https://github.com/${REPO}.git" "$TMP_DIR/src"
    cargo build --release --manifest-path "$TMP_DIR/src/Cargo.toml"
    rm -f "$INSTALL_DIR/rustybattery"
    cp "$TMP_DIR/src/target/release/rustybattery" "$INSTALL_DIR/rustybattery"
fi

# Create `battery` shortcut symlink
ln -sf "$INSTALL_DIR/rustybattery" "$INSTALL_DIR/battery"

# Ensure ~/.local/bin is in user's shell PATH
add_path_if_missing() {
    local rc_file="$1"
    if [ -f "$rc_file" ] && ! grep -q ".local/bin" "$rc_file" 2>/dev/null; then
        printf '\n# RustyBattery\nexport PATH="$HOME/.local/bin:$PATH"\n' >> "$rc_file"
        printf "\033[1;32m✔ Added ~/.local/bin to %s\033[0m\n" "$rc_file"
    fi
}

add_path_if_missing "$HOME/.bashrc"
add_path_if_missing "$HOME/.zshrc"

# Install and enable the 24/7 systemd user background service
"$INSTALL_DIR/rustybattery" service install || true

printf "\n\033[1;32m🎉 RustyBattery installed successfully!\033[0m\n"
printf "  • Launch TUI:         \033[1;36mbattery\033[0m\n"
printf "  • Optimize Battery:   \033[1;36mbattery optimize\033[0m\n"
printf "  • Quick Summary:      \033[1;36mbattery summary\033[0m\n"
printf "  • Waybar/Polybar:     \033[1;36mbattery statusbar --print-config waybar\033[0m\n"
