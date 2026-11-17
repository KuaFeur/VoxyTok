# Integration guide

VoxyTok is a webhook service, not a Discord plugin. It does not need to know which chat platform or user initiated a connection.

## Recommended integration flow

1. Your application backend calls `POST /api/v1/oauth/tiktok/start` with its VoxyTok API token.
2. Your application sends the returned `authorization_url` to the person who wants to connect TikTok.
3. TikTok redirects the browser to VoxyTok's registered callback.
4. VoxyTok validates OAuth state, stores encrypted tokens and returns a confirmation page as JSON.
5. The connected account appears in `GET /api/v1/creators`.
6. VoxyTok polls the official Display API and sends new publication events to the single webhook configured for this instance.
7. Your application verifies the signature, deduplicates the delivery and performs its own action.

The application owns all platform-specific identity and authorization. Do not add Discord user IDs, guild IDs or channel IDs to VoxyTok's core API.

## Request an authorization link

```js
const response = await fetch(
  `${process.env.VOXYTOK_URL}/api/v1/oauth/tiktok/start`,
  {
    method: "POST",
    headers: {
      Authorization: `Bearer ${process.env.VOXYTOK_API_TOKEN}`,
    },
  },
);

if (!response.ok) throw new Error(`VoxyTok returned ${response.status}`);
const { authorization_url } = await response.json();
// Send authorization_url to the creator as a private link.
```

## Verify webhook signatures

Use the raw request bytes. Parsing and re-serializing JSON before verification changes the bytes and invalidates the signature.

Node.js example:

```js
import crypto from "node:crypto";

export function verifyVoxyTokSignature(rawBody, header, secret) {
  if (!header?.startsWith("sha256=")) return false;

  const received = Buffer.from(header.slice("sha256=".length), "hex");
  const expected = crypto
    .createHmac("sha256", secret)
    .update(rawBody)
    .digest();

  return received.length === expected.length
    && crypto.timingSafeEqual(received, expected);
}
```

If `WEBHOOK_SECRET` is not configured, no signature header is sent. For production, configure a strong secret and reject unsigned requests.

## Delivery and idempotency

Webhook delivery is at-least-once. A receiver may see the same event more than once if it does not acknowledge a request or if the sender restarts during delivery.

Persist each `Idempotency-Key` in your application's own database. Check it before performing the side effect, and record successful processing. An in-memory set is not sufficient across restarts or multiple receiver processes.

Return a successful 2xx response only after your application has accepted the event. Return a non-2xx response for transient failures so VoxyTok can retry.

## Discord.js example

See [examples/discord.js](../examples/discord.js/) for a minimal Discord.js v14 example. The command only displays the TikTok authorization URL; the webhook receiver posts incoming videos to a configured channel. VoxyTok itself remains unaware of Discord.
