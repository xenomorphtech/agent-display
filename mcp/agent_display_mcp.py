#!/usr/bin/env python3

import json
import os
from pathlib import Path
import ssl
import sys
import urllib.error
import urllib.request
from urllib.parse import parse_qsl, urlencode, urlparse, urlunparse


SERVER_URL = os.environ.get("AGENT_DISPLAY_SERVER_URL", "https://127.0.0.1:3080")
PROTOCOL_VERSION = "2025-03-26"

TOOLS = [
    {
        "name": "push_markdown",
        "description": "Push a markdown document to the local Agent Display server.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "content": {"type": "string"},
                "source": {
                    "type": "string",
                    "description": "Short label describing where the markdown came from.",
                },
            },
            "required": ["title", "content"],
            "additionalProperties": False,
        },
    },
    {
        "name": "list_items",
        "description": "List recent items from the local Agent Display server.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "Maximum number of items to return.",
                }
            },
            "additionalProperties": False,
        },
    },
]


def load_api_key() -> str | None:
    key = os.environ.get("AGENT_DISPLAY_API_KEY") or os.environ.get("API_KEY")
    if key:
        key = key.strip()
        if key:
            return key

    api_key_path = Path(".api_key")
    if api_key_path.exists():
        key = api_key_path.read_text().strip()
        if key:
            return key

    return None


def is_loopback_host(hostname: str | None) -> bool:
    return hostname in {"localhost", "127.0.0.1", "::1"}


def request_context():
    parsed = urlparse(SERVER_URL)
    if parsed.scheme == "https" and is_loopback_host(parsed.hostname):
        return ssl._create_unverified_context()
    return None


API_KEY = load_api_key()
TLS_CONTEXT = request_context()


def write_message(message: dict) -> None:
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def send_result(request_id, result: dict) -> None:
    write_message({"jsonrpc": "2.0", "id": request_id, "result": result})


def send_error(request_id, code: int, message: str, data=None) -> None:
    payload = {
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": code, "message": message},
    }
    if data is not None:
        payload["error"]["data"] = data
    write_message(payload)


def tool_text(text: str, structured_content=None, is_error: bool = False) -> dict:
    result = {
        "content": [{"type": "text", "text": text}],
        "isError": is_error,
    }
    if structured_content is not None:
        result["structuredContent"] = structured_content
    return result


def build_url(path: str) -> str:
    parsed = urlparse(SERVER_URL)
    query = dict(parse_qsl(parsed.query, keep_blank_values=True))
    if API_KEY and "api_key" not in query:
        query["api_key"] = API_KEY
    return urlunparse(parsed._replace(path=path, params="", query=urlencode(query), fragment=""))


def http_json(method: str, path: str, body=None):
    data = None if body is None else json.dumps(body).encode("utf-8")
    request = urllib.request.Request(
        build_url(path),
        data=data,
        method=method,
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=10, context=TLS_CONTEXT) as response:
        return json.loads(response.read().decode("utf-8"))


def handle_initialize(request_id, params: dict) -> None:
    protocol_version = params.get("protocolVersion", PROTOCOL_VERSION)
    send_result(
        request_id,
        {
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {
                "name": "agent-display-mcp",
                "title": "Agent Display MCP",
                "version": "0.1.0",
            },
            "protocolVersion": protocol_version,
        },
    )


def handle_tools_call(request_id, params: dict) -> None:
    name = params.get("name")
    arguments = params.get("arguments") or {}

    if name == "push_markdown":
        title = arguments.get("title")
        content = arguments.get("content")
        source = arguments.get("source", "agent_display_mcp")
        if not title or not content:
            send_result(
                request_id,
                tool_text(
                    "Missing required fields: title and content are required.",
                    {"ok": False},
                    is_error=True,
                ),
            )
            return

        item = http_json(
            "POST",
            "/push",
            {
                "title": title,
                "content": content,
                "content_type": "markdown",
                "source": source,
            },
        )
        send_result(
            request_id,
            tool_text(
                f"Pushed markdown item '{item['title']}' ({item['id']}).",
                {"ok": True, "item": item},
            ),
        )
        return

    if name == "list_items":
        items = http_json("GET", "/items")
        limit = arguments.get("limit", 10)
        limited = items[:limit]
        send_result(
            request_id,
            tool_text(
                f"Fetched {len(limited)} item(s) from Agent Display.",
                {"items": limited},
            ),
        )
        return

    send_error(request_id, -32601, f"Unknown tool: {name}")


def handle_request(message: dict) -> None:
    request_id = message.get("id")
    method = message.get("method")
    params = message.get("params") or {}

    if method == "initialize":
        handle_initialize(request_id, params)
        return
    if method == "tools/list":
        send_result(request_id, {"tools": TOOLS, "nextCursor": None})
        return
    if method == "tools/call":
        try:
            handle_tools_call(request_id, params)
        except urllib.error.URLError as error:
            send_result(
                request_id,
                tool_text(
                    f"Agent Display server is unreachable at {SERVER_URL}: {error}",
                    {"ok": False, "serverUrl": SERVER_URL},
                    is_error=True,
                ),
            )
        except Exception as error:
            send_result(
                request_id,
                tool_text(
                    f"Unexpected MCP error: {error}",
                    {"ok": False},
                    is_error=True,
                ),
            )
        return
    if method == "ping":
        send_result(request_id, {})
        return
    if method.startswith("notifications/"):
        return

    send_error(request_id, -32601, f"Method not found: {method}")


def main() -> int:
    for raw_line in sys.stdin:
        line = raw_line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError as error:
            send_error(None, -32700, f"Parse error: {error}")
            continue
        if "method" not in message:
            continue
        handle_request(message)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
