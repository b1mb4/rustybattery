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
SKIP_VERIFY=0
ASSUME_YES=0
for arg in "$@"; do
    case "$arg" in
        --no-service) NO_SERVICE=1 ;;
        --no-path) NO_PATH=1 ;;
        --from-source) FROM_SOURCE=1 ;;
        --skip-verify) SKIP_VERIFY=1 ;;
        -y|--yes) ASSUME_YES=1 ;;
    esac
done

INSTALLED_PREBUILT=0
if [ "$FROM_SOURCE" -eq 0 ] && command -v curl >/dev/null 2>&1; then
    printf "Checking for pre-built binary (%s)...\n" "$ARCH"
    TARBALL_NAME="rustybattery-linux-${ARCH}.tar.gz"
    if curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/$TARBALL_NAME" 2>/dev/null; then
        if [ "$SKIP_VERIFY" -eq 0 ]; then
            if command -v sha256sum >/dev/null 2>&1; then
                SUM_CMD="sha256sum -c -"
            elif command -v shasum >/dev/null 2>&1; then
                SUM_CMD="shasum -a 256 -c -"
            else
                printf "\033[1;31m✖ Cannot verify release checksum (sha256sum/shasum not found). Re-run with --skip-verify or --from-source.\033[0m\n" >&2
                exit 1
            fi

            SUMS_URL="https://github.com/${REPO}/releases/latest/download/SHA256SUMS"
            if ! curl -fsSL "$SUMS_URL" -o "$TMP_DIR/SHA256SUMS" 2>/dev/null; then
                printf "\033[1;31m✖ Failed to download SHA256SUMS; refusing unverified binary (use --skip-verify to bypass).\033[0m\n" >&2
                exit 1
            fi
            MATCHED_LINE="$(grep -E "[ *]${TARBALL_NAME}$" "$TMP_DIR/SHA256SUMS" | head -n1 || true)"
            if [ -z "$MATCHED_LINE" ]; then
                printf "\033[1;31m✖ Checksum entry for %s missing from SHA256SUMS (use --skip-verify or --from-source).\033[0m\n" "$TARBALL_NAME" >&2
                exit 1
            fi
            if ! (cd "$TMP_DIR" && printf '%s\n' "$MATCHED_LINE" | $SUM_CMD >/dev/null); then
                printf "\033[1;31m✖ SHA256 verification FAILED for %s; aborting installation.\033[0m\n" "$TARBALL_NAME" >&2
                exit 1
            fi
            printf "\033[1;32m✔ Verified SHA256 checksum.\033[0m\n"
        else
            printf "\033[1;33m⚠ Skipping checksum verification (--skip-verify).\033[0m\n"
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
        if [ "$ASSUME_YES" -eq 0 ]; then
            if [ -t 0 ]; then
                printf "Rust/Cargo not found. Install Rust toolchain via https://sh.rustup.rs? [y/N]: "
                read -r reply
                case "$reply" in
                    y|Y|yes|YES) ;;
                    *)
                        printf "Aborted. Install Rust/Cargo manually or re-run with --yes.\n" >&2
                        exit 1
                        ;;
                esac
            else
                printf "\033[1;31m✖ Rust/Cargo not found and stdin is non-interactive; refusing unattended rustup install.\033[0m\n" >&2
                printf "Install Rust/Cargo first or re-run with '--yes' to consent to rustup.\n" >&2
                exit 1
            fi
        fi
        printf "\033[1;33mInstalling Rust/Cargo via rustup...\033[0m\n"
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
