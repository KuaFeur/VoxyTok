# Discord.js v14 example

This example demonstrates a generic VoxyTok integration without adding Discord concepts to VoxyTok itself.

## What it does

- Registers `/tiktok-connect`.
- Requests an OAuth URL from VoxyTok and sends it privately to the Discord user.
- Receives signed VoxyTok webhooks over HTTP.
- Verifies HMAC-SHA256 using the raw request body.
- Posts new TikTok videos to one configured Discord channel.
- Persists processed idempotency keys in PostgreSQL.

## Run

1. Create a Discord application and bot; enable the bot and invite it with `applications.commands` and bot permissions to send messages and embeds.
2. Configure a VoxyTok instance with this receiver's public HTTPS URL as `WEBHOOK_URL` and a strong `WEBHOOK_SECRET`.
3. Copy `.env.example` to `.env` and fill in the values.
4. Ensure the PostgreSQL database in `DATABASE_URL` exists.
5. Install dependencies with `npm install`.
6. Start the example with `npm start`.

The receiver must be reachable by VoxyTok. Put it behind HTTPS in production. Do not expose `VOXYTOK_API_TOKEN` or `VOXYTOK_WEBHOOK_SECRET`.

The in-memory bot process is a minimal example. The receipt table persists idempotency across restarts, but exactly-once side effects cannot be guaranteed: if the process crashes after sending the Discord message and before recording the receipt, a retry may post it again. Design user-facing actions to tolerate this at-least-once delivery model.
