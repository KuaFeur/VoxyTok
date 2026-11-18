# Discord.js v14 example

This example demonstrates the VoxyTok V1 public-profile workflow.

## What it does

- Registers admin-only `/tiktok-add username`, `/tiktok-remove username` and `/tiktok-list` commands (Manage Server permission).
- Adds public TikTok profiles by username through the VoxyTok API. No TikTok OAuth or creator account linking is involved.
- Receives signed VoxyTok webhooks over HTTP.
- Verifies HMAC-SHA256 using the raw request body.
- Posts new TikTok videos to one configured Discord channel.
- Persists processed idempotency keys and local creator subscriptions in PostgreSQL.

## Run

1. Create a Discord application and bot; invite it with the `applications.commands` scope and permission to send messages and embeds.
2. Deploy one VoxyTok instance for this bot/application.
3. Configure that VoxyTok instance with this receiver's public HTTPS URL as `WEBHOOK_URL` (for example, `https://bot.example.com/voxytok`) and a strong `WEBHOOK_SECRET`.
4. Copy `.env.example` to `.env` and fill in the values.
5. Ensure the PostgreSQL database in `DATABASE_URL` exists.
6. Install dependencies with `npm install`.
7. Start the example with `npm start`.
8. Use `/tiktok-add username:creator` in Discord to begin monitoring a public profile.

The receiver must be reachable by VoxyTok. Put it behind HTTPS in production. Do not expose `VOXYTOK_API_TOKEN` or `VOXYTOK_WEBHOOK_SECRET`.

The first successful profile lookup creates a baseline and does not replay historical videos. A later detected publication is sent to Discord. Public TikTok retrieval is best-effort and may be delayed by upstream rate limits or outages.

The webhook handler verifies signatures before parsing JSON and serializes duplicate deliveries with a PostgreSQL advisory lock. Delivery is still at-least-once: a process crash after sending a Discord message but before committing the receipt can result in a duplicate after retry. Make downstream side effects tolerant of that possibility.
