# Server-side speech-to-text is opt-in, off on the hosted instance

Voice messages and video notes are transcribed by Telegram itself (`messages.transcribeAudio`), which needs Telegram Premium or its weekly trial quota. Without it, clients without a shell (Claude on the web, desktop, mobile) cannot learn what a voice message says, since `get_media` download hands out a link only an agent with `curl` can use. We allow a speech-to-text model run by tg-mcp itself (whisper.cpp), but only as an opt-in for self-hosters: a separate `-stt` image built with the `stt` cargo feature, enabled at runtime by pointing `TG_MCP_STT_MODEL` at a model file the operator downloads. The hosted instance does not enable it. Audio never leaves the server for a third party, but decoding and recognising other people's speech costs CPU and memory on a small shared VPS and adds native dependencies to a multi-arch build, so it stays out of the default image and the hosted service.

## Considered Options

- Never: users without Premium and without a shell simply can't hear voice messages through us.
- On for the hosted instance with limits: needs hardware headroom we haven't measured; revisit with real numbers.
- A third-party STT API: rejected outright, it sends other people's voices to another company (ADR-0004's spirit, #33).

## Consequences

- `transcribe` tries Telegram first and falls back to the server model only when Telegram can't transcribe and STT is enabled; results say `transcribed_by: "telegram" | "server"`.
- Bounds: audio up to 5 minutes, one transcription at a time per server with a short queue (3), decoded in memory only, never written to disk or logged.
- The default image and its supply chain stay free of audio and ML native libraries.
