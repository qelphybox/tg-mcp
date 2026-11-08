#!/usr/bin/env python3
"""Manual check of refresh Token reuse detection (#16) against a running server.

Two logins through the real Login flow in your browser, each with its own
OAuth client:

1. Reuse: refreshes twice with the same refresh Token within the 60 s grace
   window (both work), tries a forged secret (revokes nothing), waits out the
   window, then presents the old refresh Token again: the Grant is revoked, so
   every Token of it is dead, and its Telegram session (held by a Live client,
   since whoami was just called) is logged out.
2. Foreign client: presents a fresh login's refresh Token with another
   client_id: the Grant is revoked the same way.

Each login adds "Unofficial MCP Server" to Telegram -> Settings -> Devices; both
must be gone at the end. No dependencies beyond Python 3. Tokens are never
printed.

    python3 scripts/refresh-reuse-check.py                       # production
    python3 scripts/refresh-reuse-check.py http://localhost:8080 # local server
"""

import base64
import hashlib
import http.server
import importlib.util
import json
import os
import secrets
import sys
import time
import urllib.parse
import webbrowser

# The HTTP, MCP and redirect helpers of the revocation check (same BASE argument),
# loaded without leaving a __pycache__ in scripts/.
sys.dont_write_bytecode = True
_spec = importlib.util.spec_from_file_location(
    "revoke_check", os.path.join(os.path.dirname(os.path.abspath(__file__)), "revoke-check.py"))
rc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rc)

GRACE_S = 60
INVALID_GRANT = {"error": "invalid_grant", "error_description": "invalid, used or expired refresh_token"}


def register(name, redirect):
    status, _, text = rc.request("POST", "/register", body={"client_name": name, "redirect_uris": [redirect]})
    if status != 201:
        sys.exit(f"/register failed: {status} {text}")
    return json.loads(text)["client_id"]


def login(step):
    """Register a client and log in through the browser: (client_id, token response)."""
    server = http.server.HTTPServer(("127.0.0.1", 0), http.server.BaseHTTPRequestHandler)
    redirect = f"http://127.0.0.1:{server.server_address[1]}/callback"
    client_id = register("refresh-reuse-check (manual test)", redirect)
    verifier = secrets.token_urlsafe(48)
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
    state = secrets.token_urlsafe(16)
    url = rc.BASE + "/authorize?" + urllib.parse.urlencode({
        "response_type": "code", "client_id": client_id, "redirect_uri": redirect,
        "code_challenge": challenge, "code_challenge_method": "S256",
        "state": state, "resource": rc.BASE + "/mcp",
    })
    print(f"{step} Log in in the browser (QR, password if asked, then Allow):\n\n   {url}\n")
    webbrowser.open(url)
    code = rc.wait_for_code(server, state)
    status, _, text = rc.request("POST", "/token", form={
        "grant_type": "authorization_code", "code": code, "client_id": client_id,
        "redirect_uri": redirect, "code_verifier": verifier, "resource": rc.BASE + "/mcp"})
    if status != 200:
        sys.exit(f"/token failed: {status} {text}")
    return client_id, json.loads(text)


def refresh(client_id, refresh_token):
    """(status, parsed body) of a refresh_token grant."""
    status, _, text = rc.request("POST", "/token", form={
        "grant_type": "refresh_token", "refresh_token": refresh_token, "client_id": client_id})
    try:
        return status, json.loads(text)
    except ValueError:
        return status, {"raw": text[:200]}


def forged(token):
    """The Token with the first character of its secret changed."""
    token_id, secret = token.split(".", 1)
    return f"{token_id}.{'B' if secret[0] == 'A' else 'A'}{secret[1:]}"


def plain_invalid_grant(label, result):
    status, body = result
    return rc.check(f"{label}: 400 invalid_grant, the usual text (got {status} {body.get('error')})",
                    status == 400 and body == INVALID_GRANT)


def whoami_status(label, access, expected):
    status, _ = rc.whoami(access)
    return rc.check(f"{label} -> {expected} on /mcp (got {status})", status == expected)


def reuse():
    client_id, first = login("1.")
    results = [whoami_status("first access Token", first["access_token"], 200)]

    status, second = refresh(client_id, first["refresh_token"])
    results.append(rc.check(f"refresh with the first refresh Token -> 200 (got {status})", status == 200))
    status, third = refresh(client_id, first["refresh_token"])
    results.append(rc.check(f"the same refresh Token again, within 60 s -> 200 (got {status})", status == 200))
    if not all(results):
        sys.exit("cannot go on without both pairs")
    results.append(rc.check("the two pairs differ", second["refresh_token"] != third["refresh_token"]))
    results.append(whoami_status("second access Token", second["access_token"], 200))
    results.append(whoami_status("third access Token", third["access_token"], 200))

    print(f"\n   Waiting {GRACE_S + 5} s for the grace window to end...")
    time.sleep(GRACE_S + 5)
    results.append(plain_invalid_grant("forged secret for the used refresh Token",
                                       refresh(client_id, forged(first["refresh_token"]))))
    results.append(whoami_status("second access Token after the forgery", second["access_token"], 200))

    stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    results.append(plain_invalid_grant("the used refresh Token after 60 s",
                                       refresh(client_id, first["refresh_token"])))
    for name, tokens in (("first", first), ("second", second), ("third", third)):
        results.append(whoami_status(f"{name} access Token after the reuse", tokens["access_token"], 401))
    results.append(plain_invalid_grant("the newest refresh Token after the reuse",
                                       refresh(client_id, third["refresh_token"])))
    print(f"   Reuse presented at {stamp} (UTC).")
    return results


def foreign_client():
    client_id, tokens = login("\n2.")
    results = [whoami_status("access Token", tokens["access_token"], 200)]
    other = register("refresh-reuse-check (another client)", "http://127.0.0.1:9/callback")
    stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    results.append(plain_invalid_grant("the refresh Token with another client_id",
                                       refresh(other, tokens["refresh_token"])))
    results.append(whoami_status("access Token after that", tokens["access_token"], 401))
    results.append(plain_invalid_grant("the refresh Token with its own client_id",
                                       refresh(client_id, tokens["refresh_token"])))
    print(f"   Foreign client presented at {stamp} (UTC).")
    return results


def main():
    results = reuse()
    input("\n   Press Enter to go on with the foreign-client check (a second login)... ")
    results += foreign_client()
    print("\n3. Now check Telegram -> Settings -> Devices: no 'Unofficial MCP Server' from these two logins.")
    sys.exit(0 if all(results) else 1)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nStopped.")
