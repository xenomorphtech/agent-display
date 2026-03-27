# Agent Display

Small Rust workspace for sending content to a local server and viewing it in a desktop app.

## Binaries

- `llm-viewer-server`: local HTTPS + secure WebSocket server on `127.0.0.1:3080`
- `llm-viewer`: desktop viewer that fetches existing items and listens for live updates

## Run It

Start the server:

```bash
cargo run -p llm-viewer-server
```

If `--cert` and `--key` are not provided, the server generates a self-signed certificate automatically and stores it under `~/.local/share/llm-viewer-server/tls` by default. It also writes the API key to `.api_key` on first start. The default bind address is `0.0.0.0:3080`.

Start the viewer in another terminal:

```bash
cargo run -p llm-viewer
```

## System Service

Install the server as a systemd service:

```bash
./scripts/install-systemd-service.sh
```

The installed unit is `llm-viewer-server.service`. It runs as user `sdancer`, binds `0.0.0.0:3080` by default, starts after `network-online.target`, declares `After=spacetimedb.service`, and also waits for the configured `STDB_SERVER` endpoint to accept connections before launching the server process. When `ufw` or `firewalld` is active, the installer also opens `3080/tcp`.

## Push Markdown

Send a markdown item to the server:

```bash
API_KEY=$(cat .api_key)
curl -k -X POST "https://127.0.0.1:3080/push?api_key=${API_KEY}" \
  -H 'content-type: application/json' \
  -d '{
    "title": "Markdown test",
    "content": "# Hello\n\n- one\n- two\n\n`code`",
    "content_type": "markdown",
    "source": "curl"
  }'
```

`content_type` must be lowercase JSON: `"markdown"` or `"html"`.

## Endpoints

- `POST /push` creates a new item
- `GET /items` returns all items, newest first
- `GET /items/{id}` returns one item
- `GET /ws` streams new items over WebSocket

## MCP Helper

This repo also includes a tiny stdio MCP server at `mcp/agent_display_mcp.py`.

It exposes:

- `push_markdown`: push markdown content into the display server
- `list_items`: fetch the latest items from the display server

Register it in Codex:

```bash
codex mcp add agent_display -- python3 /home/sdancer/projects/agent-display/mcp/agent_display_mcp.py
```

Then ask Codex to use `agent_display.push_markdown` to send content to the server.
