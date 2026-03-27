#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_DIR"

cargo build --release -p llm-viewer-server
sudo install -m 0755 target/release/llm-viewer-server /usr/local/bin/llm-viewer-server
sudo install -m 0644 systemd/llm-viewer-server.service /etc/systemd/system/llm-viewer-server.service

if command -v ufw >/dev/null 2>&1 && sudo systemctl is-active --quiet ufw; then
  sudo ufw allow 3080/tcp comment 'llm-viewer-server'
elif command -v firewall-cmd >/dev/null 2>&1 && sudo systemctl is-active --quiet firewalld; then
  sudo firewall-cmd --permanent --add-port=3080/tcp
  sudo firewall-cmd --reload
fi

sudo systemctl daemon-reload
sudo systemctl enable llm-viewer-server.service
sudo systemctl restart llm-viewer-server.service
