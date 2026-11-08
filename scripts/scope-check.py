#!/usr/bin/env python3
"""Manual check of Scopes (#41, #43) against a running server, with its own OAuth client.

Logs in through the real Login flow in your browser, asking for the Scopes you
name (`-` asks for none). On the Consent screen, tick or untick "Write Drafts"
so that the Grant gets EXPECTED. Then it checks the token response's `scope`,
that a refresh asking for every Scope keeps it, what the server's instructions
say, which tools are listed (save_draft only with `draft`, get_drafts always),
that a Grant without `draft` can't call save_draft (nothing is written: the call
is refused before Telegram) and that whoami works, and finally revokes the
Grant (so "Unofficial MCP Server" leaves Telegram -> Settings -> Devices).

No dependencies beyond Python 3. Tokens are never printed.

    python3 scripts/scope-check.py - read                       # production
    python3 scripts/scope-check.py "read draft" "read draft"
    python3 scripts/scope-check.py "read draft" read http://localhost:8080
"""

import base64
import hashlib
import http.server
import importlib.util
import json
import os
import secrets
import sys
import urllib.parse
import webbrowser

if len(sys.argv) not in (3, 4):
    sys.exit(__doc__)
ASKED, EXPECTED = sys.argv[1], sys.argv[2]
# The HTTP, MCP and redirect helpers of the revocation check read the server's
# URL from argv[1]; loaded without leaving a __pycache__ in scripts/.
sys.argv = sys.argv[:1] + sys.argv[3:]
sys.dont_write_bytecode = True
_spec = importlib.util.spec_from_file_location(
    "revoke_check", os.path.join(os.path.dirname(os.path.abspath(__file__)), "revoke-check.py"))
rc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rc)


def login():
    server = http.server.HTTPServer(("127.0.0.1", 0), http.server.BaseHTTPRequestHandler)
    redirect = f"http://127.0.0.1:{server.server_address[1]}/callback"
    status, _, text = rc.request("POST", "/register", body={
        "client_name": "scope-check (manual test)", "redirect_uris": [redirect]})
    if status != 201:
        sys.exit(f"/register failed: {status} {text}")
    client_id = json.loads(text)["client_id"]
    verifier = secrets.token_urlsafe(48)
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
    state = secrets.token_urlsafe(16)
    params = {
        "response_type": "code", "client_id": client_id, "redirect_uri": redirect,
        "code_challenge": challenge, "code_challenge_method": "S256",
        "state": state, "resource": rc.BASE + "/mcp",
    }
    if ASKED != "-":
        params["scope"] = ASKED
    url = rc.BASE + "/authorize?" + urllib.parse.urlencode(params)
    tick = "tick" if "draft" in EXPECTED.split() else "untick"
    print(f"1. Log in in the browser (QR, password if asked), {tick} 'Write Drafts', then Allow:\n\n   {url}\n")
    webbrowser.open(url)
    code = rc.wait_for_code(server, state)
    status, _, text = rc.request("POST", "/token", form={
        "grant_type": "authorization_code", "code": code, "client_id": client_id,
        "redirect_uri": redirect, "code_verifier": verifier, "resource": rc.BASE + "/mcp"})
    if status != 200:
        sys.exit(f"/token failed: {status} {text}")
    return client_id, json.loads(text)


def mcp_session(access):
    """(instructions, tools, session) as an MCP client sees them on initialize and tools/list."""
    status, session, answer = rc.mcp_post(access, {
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "scope-check", "version": "1"}},
    })
    if status != 200:
        sys.exit(f"initialize failed: {status}")
    instructions = (answer or {}).get("result", {}).get("instructions", "")
    rc.mcp_post(access, {"jsonrpc": "2.0", "method": "notifications/initialized"}, session)
    _, _, answer = rc.mcp_post(access, {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}, session)
    tools = (answer or {}).get("result", {}).get("tools", [])
    return instructions, tools, session


def main():
    client_id, tokens = login()
    results = [rc.check(f"token response scope = {EXPECTED!r} (got {tokens.get('scope')!r})",
                        tokens.get("scope") == EXPECTED)]

    status, _, text = rc.request("POST", "/token", form={
        "grant_type": "refresh_token", "refresh_token": tokens["refresh_token"],
        "client_id": client_id, "scope": "read draft send organize"})
    refreshed = json.loads(text) if status == 200 else {}
    results.append(rc.check(
        f"a refresh asking for every Scope keeps {EXPECTED!r} (got {status} {refreshed.get('scope')!r})",
        status == 200 and refreshed.get("scope") == EXPECTED))
    access = refreshed.get("access_token", tokens["access_token"])
    refresh = refreshed.get("refresh_token", tokens["refresh_token"])

    instructions, tools, session = mcp_session(access)
    print(f"\n2. The server's instructions:\n\n   {instructions}\n")
    if EXPECTED == "read":
        results.append(rc.check("instructions say read-only",
                                "Read-only access to the connected Telegram account" in instructions))
    else:
        results.append(rc.check("instructions drop read-only and name Drafts",
                                "Read-only" not in instructions and "write Drafts" in instructions
                                and "you cannot send, edit or delete messages" in instructions))
    names = sorted(t["name"] for t in tools)
    print(f"   tools/list ({len(names)}): {', '.join(names)}\n")
    results.append(rc.check("whoami is listed", "whoami" in names))
    results.append(rc.check("get_drafts is listed", "get_drafts" in names))
    drafts = "draft" in EXPECTED.split()
    results.append(rc.check(f"save_draft is {'' if drafts else 'not '}listed",
                            ("save_draft" in names) == drafts))
    if not drafts:
        # Refused by the Scope check before anything reaches Telegram.
        _, _, answer = rc.mcp_post(access, {
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "save_draft", "arguments": {"chat_id": "chat:1", "text": "scope check"}},
        }, session)
        error = (answer or {}).get("error", {}).get("message", "")
        results.append(rc.check(f"calling save_draft is refused for the missing Scope ({error[:60]!r}...)",
                                "needs the `draft` Scope" in error))
    status, me = rc.whoami(access)
    results.append(rc.check(f"whoami -> 200 with the account id (got {status})",
                            status == 200 and bool(me and me.get("id"))))

    input("3. Press Enter to revoke this Grant (Ctrl-C to keep it)... ")
    status, _, _ = rc.request("POST", "/revoke", form={
        "token": refresh, "token_type_hint": "refresh_token", "client_id": client_id})
    results.append(rc.check(f"/revoke answered 200 (got {status})", status == 200))
    status, _ = rc.whoami(access)
    results.append(rc.check(f"access Token -> 401 on /mcp after revoking (got {status})", status == 401))
    sys.exit(0 if all(results) else 1)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nStopped; the Grant was not revoked.")
