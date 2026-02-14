#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

cd "${REPO_ROOT}"

echo "Building ESS release binary..."
cargo build --release

BIN_DIR="${HOME}/.local/bin"
mkdir -p "${BIN_DIR}"
cp "${REPO_ROOT}/target/release/ess" "${BIN_DIR}/ess"
chmod +x "${BIN_DIR}/ess"

echo "Installed: ${BIN_DIR}/ess"

CONFIG_DIR="${HOME}/.ess"
CONFIG_FILE="${CONFIG_DIR}/config.toml"
mkdir -p "${CONFIG_DIR}"

if [[ ! -f "${CONFIG_FILE}" ]]; then
  cat > "${CONFIG_FILE}" <<'TOML'
[general]
default_scope = "all"

[accounts]
# Add account blocks here, for example:
# [accounts.work]
# account_id = "acc-work"
# email = "you@company.com"
# type = "professional"
# tenant_id = "your-tenant-id"
TOML
  echo "Created default config: ${CONFIG_FILE}"
else
  echo "Config already exists: ${CONFIG_FILE}"
fi

if [[ ":${PATH}:" != *":${BIN_DIR}:"* ]]; then
  echo "Note: ${BIN_DIR} is not currently in PATH. Add it in your shell profile."
fi

echo "ESS install complete."
