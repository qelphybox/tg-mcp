---
title: Support
permalink: /support/
---

# Support

tg-mcp is an unofficial, open-source MCP server that gives AI clients read access to your Telegram account and, only if you allow it, lets them write Drafts that you send yourself. It is not affiliated with or endorsed by Telegram. This page covers the hosted instance at `https://tgmcp.qelphybox.space/mcp`.

## Get help

| What | Where |
|---|---|
| Bug or something not working | [GitHub Issues](https://github.com/qelphybox/tg-mcp/issues/new) |
| Feature request or question | [GitHub Issues](https://github.com/qelphybox/tg-mcp/issues) |
| Security vulnerability | [Report privately](https://github.com/qelphybox/tg-mcp/security/advisories/new). Please don't open a public issue. |
| Data deletion or privacy request | [TODO: contact email], see the [Privacy Policy](../privacy/) |
| Anything else | [TODO: contact email] |

This is a project maintained by one person. We answer on a best-effort basis, usually within a few days.

**Never send us** your Telegram login codes, cloud password, OAuth tokens or screenshots of your private chats. We never need them. In bug reports, the tool name, the error text, your AI client and its version, and the approximate time (with timezone) are enough.

## Connect

**Claude Code**

```sh
claude mcp add --transport http telegram https://tgmcp.qelphybox.space/mcp
```

Then run `/mcp` → `telegram` → **Authenticate**.

**Claude (web, desktop):** Settings → Connectors → Add custom connector → `https://tgmcp.qelphybox.space/mcp`.

**Other MCP clients:** add the same URL as a streamable-HTTP server. The client discovers OAuth on its own.

Your browser then opens the login page:

1. Scan the QR code with Telegram on your phone: **Settings → Devices → Link Desktop Device**.
2. If your account has a cloud password, type it on the same page. It goes only to this server, never to the AI client.
3. On the consent screen, choose what to allow and click **Allow**. Reading is always included. Leave **Write Drafts** and **Send messages** unticked for a read-only connection.

## Disconnect

In Telegram, go to **Settings → Devices** and end the **Unofficial MCP Server** session. This logs the server out at once.

Removing the connector in your AI client alone is **not** enough: most clients just forget their tokens, and the server can't log out a session it can no longer decrypt. Such a session stops working on its own within 30 days. See [Privacy Policy → Your choices](../privacy/#your-choices-and-rights).

## Common problems

**The QR code expired or the login page says to start again.**
A login is valid for 10 minutes and doesn't survive a server restart. Start the authentication again from your AI client.

**"Too many logins" or "temporarily unavailable" on the login page.**
Only a few logins can run at once, per network and server-wide. Wait a minute and try again.

**My AI client says 401 / unauthorized / needs to re-authenticate.**
The session was ended in Telegram, the connection expired after 30 days without use, or the client presented an old refresh token (which revokes the connection for safety). Authenticate again.

**A tool fails with "retry in N s".**
Telegram asked us to slow down (`FLOOD_WAIT`), or you hit the writes budget (20 writes per 10 minutes by default). We never retry around Telegram's limits, because that is how accounts get restricted. Wait the stated time.

**The AI can't write Drafts, or a tool I expected isn't there.**
Tools only appear for the permissions you granted on the consent screen. Permissions can't be changed on an existing connection: remove the connector, add it again, and tick what you need.

**The AI can't understand voice messages.**
Transcription is done by Telegram itself and needs Telegram Premium or Telegram's weekly trial quota. Clients with a shell, such as Claude Code, can also download the audio.

**A chat folder isn't supported.**
Folders defined by rules (for example "all unread", "all groups") aren't supported yet. Folders that list specific chats are.

**The AI quotes something strange from a chat.**
Messages, names and chat titles are written by other people and are passed to the AI as data. If a message looks like it is steering the AI, please report it with the tool name and the time, but without the content.

## Self-hosting

Running your own instance is supported and documented in the [README](https://github.com/qelphybox/tg-mcp#self-hosting). Issues about self-hosted instances are welcome on GitHub, but we can't access or debug your server.

## Status

Planned maintenance and deploys restart the server. Open AI client sessions reconnect on their own, and a login in progress has to be started again. For outages, check [GitHub Issues](https://github.com/qelphybox/tg-mcp/issues) or open one.
