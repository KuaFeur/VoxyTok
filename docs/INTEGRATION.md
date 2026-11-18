# Integration guide

VoxyTok is a webhook service, not a Discord plugin. One VoxyTok instance belongs to one consuming bot or application and sends events to that instance's single configured webhook destination.

## Add a TikTok profile

Your backend only needs the creator's public TikTok username. There is no creator OAuth, account linking, cookie collection, video submission or per-source setup.

```js
const response = await fetch(
  `${process.env.VOXYTOK_URL}/api/v1/creators`,
  {
    method: "POST",
    headers: {
      Authorization: `Bearer ${process.env.VOXYTOK_API_TOKEN}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ username: "@creator" }),
  },
);

if (!response.ok) {
  throw new Error(`VoxyTok returned ${response.status}`);
}

const creator = await response.json();
// Persist creator.id in your own database if you need to manage this subscription.
```

The first successful source lookup creates a baseline without replaying the creator's existing catalogue. Later discoveries are sent to your webhook.

## Remove a profile

```http
DELETE /api/v1/creators/CREATOR_UUID
Authorization: Bearer YOUR_API_TOKEN
```

This removes monitoring state and associated event history.

## Verify webhook signatures

Use the raw request bytes. Parsing and re-serializing JSON before verification changes the bytes and invalidates the signature.

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

For production, configure `WEBHOOK_SECRET` and reject unsigned requests.

## Delivery and idempotency

Webhook delivery is at-least-once. Persist each `Idempotency-Key` in your application's database. Check it before performing the side effect, and record successful processing. Return a successful 2xx response only after your application has accepted the event. Return a non-2xx response for transient failures so VoxyTok can retry.

## Discord.js example

See [examples/discord.js](../examples/discord.js/) for a minimal Discord.js v14 receiver example. The consuming bot owns its commands, permissions and channel presentation. VoxyTok remains platform-agnostic.

## Detection expectations

VoxyTok polls public sources on a configurable interval (five minutes by default). The direct public-profile adapter is tried first; an RSSHub-compatible feed adapter is used as a fallback. Source-specific rate limits and errors trigger cooldowns and backoff. No public TikTok source can honestly be promised as unlimited or guaranteed real-time; a TikTok challenge or upstream outage can delay notifications.
