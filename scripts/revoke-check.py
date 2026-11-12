#!/usr/bin/env python3
"""Manual check of Disconnect (#11) against a running server, with its own OAuth client.

Logs in through the real Login flow in your browser (QR, cloud password,
Consent), calls `whoami` over MCP, then on Enter revokes the Grant through
/revoke (RFC 7009) and checks that the old Tokens are dead. You then confirm
in Telegram (Settings -> Devices) that "Unofficial MCP Server" is gone.

No dependencies beyond Python 3. Tokens are never printed.

    python3 scripts/revoke-check.py                       # production
    python3 scripts/revoke-check.py http://localhost:8080 # local server
"""

import base64
import hashlib
import http.server
import json
import secrets
import sys
import threading
import urllib.error
import urllib.parse
import urllib.request
import webbrowser

BASE = (sys.argv[1] if len(sys.argv) > 1 else "https://connector.tgmcp.dev").rstrip("/")


def request(method, path, *, form=None, body=None, headers=None):
    """(status, headers, body text) without raising on HTTP errors."""
    data, hdrs = None, dict(headers or {})
    if form is not None:
        data = urllib.parse.urlencode(form).encode()
        hdrs["Content-Type"] = "application/x-www-form-urlencoded"
    elif body is not None:
        data = json.dumps(body).encode()
        hdrs["Content-Type"] = "application/json"
    req = urllib.request.Request(BASE + path, data=data, headers=hdrs, method=method)
    try:
        with urllib.request.urlopen(req, timeout=60) as res:
            return res.status, res.headers, res.read().decode()
    except urllib.error.HTTPError as err:
        return err.code, err.headers, err.read().decode()


def mcp_post(access, message, session=None):
    headers = {
        "Authorization": f"Bearer {access}",
        "Accept": "application/json, text/event-stream",
    }
    if session:
        headers["Mcp-Session-Id"] = session
        headers["MCP-Protocol-Version"] = "2025-06-18"
    status, hdrs, text = request("POST", "/mcp", body=message, headers=headers)
    payload = None
    for line in text.splitlines():  # SSE: the JSON-RPC answer is on a data: line
        if line.startswith("data:") and line[5:].strip():
            payload = json.loads(line[5:])
    if payload is None and text.strip().startswith("{"):
        payload = json.loads(text)
    return status, hdrs.get("Mcp-Session-Id"), payload


def whoami(access):
    status, session, _ = mcp_post(access, {
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "revoke-check", "version": "1"}},
    })
    if status != 200:
        return status, None
    mcp_post(access, {"jsonrpc": "2.0", "method": "notifications/initialized"}, session)
    status, _, answer = mcp_post(access, {
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "whoami", "arguments": {}},
    }, session)
    result = (answer or {}).get("result", {})
    return status, result.get("structuredContent")


def wait_for_code(server, state):
    got = {}

    class Callback(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            query = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(self.path).query))
            if "code" not in query and "error" not in query:
                self.send_response(404)
                self.end_headers()
                return
            got.update(query)
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.end_headers()
            self.wfile.write("Done, go back to the terminal.".encode())

        def log_message(self, *_):
            pass

    server.RequestHandlerClass = Callback
    while not got:
        server.handle_request()
    if got.get("state") != state:
        sys.exit("state mismatch in the redirect, aborting")
    if "error" in got:
        sys.exit(f"authorization failed: {got['error']} ({got.get('error_description', '')})")
    return got["code"]


def check(label, ok):
    print(f"  [{'PASS' if ok else 'FAIL'}] {label}")
    return ok


def main():
    server = http.server.HTTPServer(("127.0.0.1", 0), http.server.BaseHTTPRequestHandler)
    redirect = f"http://127.0.0.1:{server.server_address[1]}/callback"

    status, _, text = request("POST", "/register", body={
        "client_name": "revoke-check (manual test)", "redirect_uris": [redirect]})
    if status != 201:
        sys.exit(f"/register failed: {status} {text}")
    client_id = json.loads(text)["client_id"]

    verifier = secrets.token_urlsafe(48)
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
    state = secrets.token_urlsafe(16)
    url = BASE + "/authorize?" + urllib.parse.urlencode({
        "response_type": "code", "client_id": client_id, "redirect_uri": redirect,
        "code_challenge": challenge, "code_challenge_method": "S256",
        "state": state, "resource": BASE + "/mcp",
    })
    print("1. Log in in the browser (QR, password if asked, then Allow):\n\n   " + url + "\n")
    webbrowser.open(url)
    code = wait_for_code(server, state)

    status, _, text = request("POST", "/token", form={
        "grant_type": "authorization_code", "code": code, "client_id": client_id,
        "redirect_uri": redirect, "code_verifier": verifier, "resource": BASE + "/mcp"})
    if status != 200:
        sys.exit(f"/token failed: {status} {text}")
    tokens = json.loads(text)
    access, refresh = tokens["access_token"], tokens["refresh_token"]

    status, me = whoami(access)
    print(f"2. Logged in, whoami -> {status}: {me}")
    print("   Telegram -> Settings -> Devices should now list 'Unofficial MCP Server'.\n")
    input("3. Press Enter to revoke the Grant (Ctrl-C to keep it)... ")

    status, _, _ = request("POST", "/revoke", form={
        "token": refresh, "token_type_hint": "refresh_token", "client_id": client_id})
    results = [check(f"/revoke answered 200 (got {status})", status == 200)]
    status, _ = whoami(access)
    results.append(check(f"old access Token -> 401 on /mcp (got {status})", status == 401))
    status, _, text = request("POST", "/token", form={
        "grant_type": "refresh_token", "refresh_token": refresh, "client_id": client_id})
    results.append(check(f"old refresh Token no longer refreshes (got {status})",
                         status == 400 and "invalid_grant" in text))
    print("\n4. Now check Telegram -> Settings -> Devices: 'Unofficial MCP Server' must be gone.")
    sys.exit(0 if all(results) else 1)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nStopped; the Grant was not revoked.")
