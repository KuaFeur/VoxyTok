import "dotenv/config";
import crypto, { randomBytes } from "node:crypto";
import express from "express";
import pg from "pg";
import {
  Client,
  EmbedBuilder,
  GatewayIntentBits,
  REST,
  Routes,
  SlashCommandBuilder,
} from "discord.js";

const required = [
  "DISCORD_TOKEN",
  "DISCORD_CLIENT_ID",
  "DISCORD_NOTIFY_CHANNEL_ID",
  "VOXYTOK_URL",
  "VOXYTOK_API_TOKEN",
  "VOXYTOK_WEBHOOK_SECRET",
  "DATABASE_URL",
];
for (const key of required) {
  if (!process.env[key]) throw new Error(`Missing environment variable: ${key}`);
}

const client = new Client({ intents: [GatewayIntentBits.Guilds] });
const pool = new pg.Pool({ connectionString: process.env.DATABASE_URL });
const app = express();

await pool.query(`
  CREATE TABLE IF NOT EXISTS voxytok_webhook_receipts (
    idempotency_key TEXT PRIMARY KEY,
    processed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
  );
  CREATE TABLE IF NOT EXISTS voxytok_oauth_requests (
    reference TEXT PRIMARY KEY,
    discord_user_id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    creator_id UUID,
    connected_at TIMESTAMPTZ
  )
`);

const command = new SlashCommandBuilder()
  .setName("tiktok-connect")
  .setDescription("Connect your TikTok account to this bot");

const rest = new REST({ version: "10" }).setToken(process.env.DISCORD_TOKEN);
const route = process.env.DISCORD_GUILD_ID
  ? Routes.applicationGuildCommands(process.env.DISCORD_CLIENT_ID, process.env.DISCORD_GUILD_ID)
  : Routes.applicationCommands(process.env.DISCORD_CLIENT_ID);

await rest.put(route, { body: [command.toJSON()] });

client.on("interactionCreate", async (interaction) => {
  if (!interaction.isChatInputCommand() || interaction.commandName !== "tiktok-connect") return;

  await interaction.deferReply({ ephemeral: true });
  try {
    const reference = randomBytes(32).toString("base64url");
    await pool.query(
      "INSERT INTO voxytok_oauth_requests (reference, discord_user_id) VALUES ($1,$2)",
      [reference, interaction.user.id],
    );
    const response = await fetch(
      `${process.env.VOXYTOK_URL}/api/v1/oauth/tiktok/start`,
      {
        method: "POST",
        headers: {
          Authorization: `Bearer ${process.env.VOXYTOK_API_TOKEN}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ reference }),
      },
    );

    if (!response.ok) {
      throw new Error(`VoxyTok returned HTTP ${response.status}`);
    }

    const { authorization_url } = await response.json();
    await interaction.editReply({
      content: `Connect your TikTok account here: ${authorization_url}\nThe link expires in 10 minutes.`,
      allowedMentions: { parse: [] },
    });
  } catch (error) {
    console.error("Could not start TikTok OAuth:", error);
    await interaction.editReply("Unable to start TikTok authorization. Please try again later.");
  }
});

// Resolve completed OAuth attempts without giving VoxyTok any Discord identity.
async function syncOAuthConnections() {
  const pending = await pool.query(
    "SELECT reference FROM voxytok_oauth_requests WHERE creator_id IS NULL AND created_at > NOW() - INTERVAL '1 day' ORDER BY created_at LIMIT 50",
  );
  for (const row of pending.rows) {
    try {
      const response = await fetch(
        `${process.env.VOXYTOK_URL}/api/v1/oauth/tiktok/connections/${row.reference}`,
        { headers: { Authorization: `Bearer ${process.env.VOXYTOK_API_TOKEN}` } },
      );
      if (response.status === 404) continue;
      if (!response.ok) throw new Error(`VoxyTok returned HTTP ${response.status}`);
      const status = await response.json();
      if (status.status === "connected" && status.creator_id) {
        await pool.query(
          "UPDATE voxytok_oauth_requests SET creator_id=$2,connected_at=NOW() WHERE reference=$1 AND creator_id IS NULL",
          [row.reference, status.creator_id],
        );
        console.info("A TikTok creator was linked to a local Discord user");
      } else if (["failed", "expired", "reauthorization_required"].includes(status.status)) {
        await pool.query("DELETE FROM voxytok_oauth_requests WHERE reference=$1", [row.reference]);
        if (status.status === "reauthorization_required") {
          console.info("A TikTok creator must reconnect their account before monitoring can resume");
        }
      }
    } catch (error) {
      console.error("Could not sync OAuth connection:", error);
    }
  }
}
setInterval(() => syncOAuthConnections().catch(console.error), 15_000);
await syncOAuthConnections();

// Keep the raw body: HMAC must be checked before JSON parsing.
app.post("/voxytok", express.raw({ type: "application/json", limit: "256kb" }), async (req, res) => {
  try {
    const key = req.header("Idempotency-Key");
    const signature = req.header("X-VoxyTok-Signature");
    if (!key || !signature?.startsWith("sha256=")) {
      return res.status(401).send("Missing signature or idempotency key");
    }

    const received = Buffer.from(signature.slice(7), "hex");
    const expected = crypto
      .createHmac("sha256", process.env.VOXYTOK_WEBHOOK_SECRET)
      .update(req.body)
      .digest();

    if (received.length !== expected.length || !crypto.timingSafeEqual(received, expected)) {
      return res.status(401).send("Invalid signature");
    }

    const existing = await pool.query(
      "SELECT 1 FROM voxytok_webhook_receipts WHERE idempotency_key = $1",
      [key],
    );
    if (existing.rowCount) return res.sendStatus(204);

    const event = JSON.parse(req.body.toString("utf8"));
    if (event.event !== "tiktok.video.published") return res.sendStatus(204);

    const video = event.video ?? {};
    const creator = event.creator ?? {};
    const channel = await client.channels.fetch(process.env.DISCORD_NOTIFY_CHANNEL_ID);
    if (!channel?.isTextBased() || !("send" in channel)) {
      throw new Error("Configured Discord channel is not a text channel");
    }

    const truncate = (value, limit) => String(value ?? "").slice(0, limit);
    const embed = new EmbedBuilder()
      .setTitle(truncate(video.title || "New TikTok video", 256))
      .setDescription(truncate(video.description || "A creator published a new video.", 4096))
      .addFields({
        name: "Creator",
        value: truncate(creator.display_name || creator.username || "TikTok creator", 1024),
      });

    if (video.url) embed.setURL(video.url);
    if (video.cover_image_url) embed.setImage(video.cover_image_url);
    await channel.send({ embeds: [embed] });

    // Persist after the side effect. A crash between send() and INSERT can
    // still produce a duplicate; webhook delivery is at-least-once.
    await pool.query(
      "INSERT INTO voxytok_webhook_receipts (idempotency_key) VALUES ($1) ON CONFLICT DO NOTHING",
      [key],
    );
    return res.sendStatus(204);
  } catch (error) {
    console.error("VoxyTok webhook handling failed:", error);
    return res.status(500).send("Temporary processing failure");
  }
});

app.listen(Number(process.env.PORT || 3000), () => {
  console.log("VoxyTok Discord.js example webhook receiver is listening");
});

await client.login(process.env.DISCORD_TOKEN);
