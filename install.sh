#!/usr/bin/env bash
# 🦀 RustyBattery — One-Command Universal Linux Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/b1mb4/rustybattery/main/install.sh | bash

set -euo pipefail

REPO="b1mb4/rustybattery"
INSTALL_DIR="${HOME}/.local/bin"
ARCH="$(uname -m)"

printf "\033[1;36m🦀 Installing RustyBattery — Per-App Power Tracker for Linux...\033[0m\n"

mkdir -p "$INSTALL_DIR"

DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/rustybattery-linux-${ARCH}.tar.gz"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

NO_SERVICE=0
NO_PATH=0
FROM_SOURCE=0
for arg in "$@"; do
    case "$arg" in
        --no-service) NO_SERVICE=1 ;;
        --no-path) NO_PATH=1 ;;
        --from-source) FROM_SOURCE=1 ;;
    esac
done

INSTALLED_PREBUILT=0
if [ "$FROM_SOURCE" -eq 0 ] && command -v curl >/dev/null 2>&1; then
    printf "Checking for pre-built binary (%s)...\n" "$ARCH"
    TARBALL_NAME="rustybattery-linux-${ARCH}.tar.gz"
    if curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/$TARBALL_NAME" 2>/dev/null; then
        SUMS_URL="https://github.com/${REPO}/releases/latest/download/SHA256SUMS"
        if curl -fsSL "$SUMS_URL" -o "$TMP_DIR/SHA256SUMS" 2>/dev/null && command -v sha256sum >/dev/null 2>&1; then
            if grep -q "$TARBALL_NAME" "$TMP_DIR/SHA256SUMS"; then
                (cd "$TMP_DIR" && grep "$TARBALL_NAME" SHA256SUMS | sha256sum -c - >/dev/null)
                printf "\033[1;32m✔ Verified SHA256 checksum.\033[0m\n"
            fi
        fi
        tar -xzf "$TMP_DIR/$TARBALL_NAME" -C "$TMP_DIR"
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

if [ "$NO_PATH" -eq 0 ]; then
    add_path_if_missing "$HOME/.bashrc"
    add_path_if_missing "$HOME/.zshrc"
fi

if [ "$NO_SERVICE" -eq 0 ]; then
    # Install and enable the 24/7 systemd user background service
    "$INSTALL_DIR/rustybattery" service install || true
fi
# Install local man pages (`man battery`, `man rustybattery`)
"$INSTALL_DIR/rustybattery" man --install || true

# Install bash/zsh/fish shell completions when standard user dirs exist
if [ -d "$HOME/.local/share/bash-completion/completions" ] || mkdir -p "$HOME/.local/share/bash-completion/completions" 2>/dev/null; then
    "$INSTALL_DIR/rustybattery" completions bash > "$HOME/.local/share/bash-completion/completions/battery" 2>/dev/null || true
    "$INSTALL_DIR/rustybattery" completions bash > "$HOME/.local/share/bash-completion/completions/rustybattery" 2>/dev/null || true
fi
if [ -d "$HOME/.config/fish/completions" ]; then
    "$INSTALL_DIR/rustybattery" completions fish > "$HOME/.config/fish/completions/battery.fish" 2>/dev/null || true
fi

printf "\n"
"$INSTALL_DIR/rustybattery" doctor || true

printf "\n\033[1;32m🎉 RustyBattery installed successfully!\033[0m\n"
printf "  • Launch TUI:         \033[1;36mbattery\033[0m\n"
printf "  • Optimize Battery:   \033[1;36mbattery optimize\033[0m\n"
printf "  • Quick Summary:      \033[1;36mbattery summary\033[0m\n"
printf "  • Calibrate Model:    \033[1;36mbattery calibrate\033[0m\n"
printf "  • Disable Autopilot:  \033[1;36mbattery autopilot off\033[0m\n"
printf "  • Waybar/Polybar:     \033[1;36mbattery statusbar --print-config waybar\033[0m\n"
