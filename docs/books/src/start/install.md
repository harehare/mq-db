# Install

## Using the Installation Script (Recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/harehare/mq-db/main/bin/install.sh | bash
```

The installer will:

- Download the latest release for your platform
- Verify the binary with a SHA256 checksum
- Install to `~/.local/bin/`
- Update your shell profile (bash, zsh, or fish)

After installation, restart your terminal or run:

```bash
source ~/.bashrc  # or ~/.zshrc, or ~/.config/fish/config.fish
```

## Using Cargo

```bash
cargo install mq-db
```

## From Source

```bash
# Latest development version
cargo install --git https://github.com/harehare/mq-db.git
```

## Supported Platforms

- **Linux**: x86_64, aarch64
- **macOS**: x86_64 (Intel), aarch64 (Apple Silicon)
- **Windows**: x86_64

## Verify

```bash
mq-db --version
```
