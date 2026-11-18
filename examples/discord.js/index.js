import "dotenv/config";
import crypto from "node:crypto";
import express from "express";
import pg from "pg";
import {
  Client,
  EmbedBuilder,
  GatewayIntentBits,
  PermissionFlagsBits,
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
  CREATE TABLE IF NOT EXISTS voxytok_subscriptions (
    creator_id UUID PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    added_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
  )
`);

const addCommand = new SlashCommandBuilder()
  .setName("tiktok-add")
  .setDescription("Monitor a public TikTok profile")
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addStringOption((option) =>
    option
      .setName("username")
      .setDescription("Public TikTok username, with or without @")
      .setRequired(true)
      .setMinLength(1)
      .setMaxLength(64),
  );

const removeCommand = new SlashCommandBuilder()
  .setName("tiktok-remove")
  .setDescription("Stop monitoring a TikTok profile")
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addStringOption((option) =>
    option
      .setName("username")
      .setDescription("TikTok username to remove")
      .setRequired(true)
      .setMinLength(1)
      .setMaxLength(64),
  );

const listCommand = new SlashCommandBuilder()
  .setName("tiktok-list")
  .setDescription("List monitored TikTok profiles")
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild);

const rest = new REST({ version: "10" }).setToken(process.env.DISCORD_TOKEN);
const route = process.env.DISCORD_GUILD_ID
  ? Routes.applicationGuildCommands(process.env.DISCORD_CLIENT_ID, process.env.DISCORD_GUILD_ID)
  : Routes.applicationCommands(process.env.DISCORD_CLIENT_ID);

await rest.put(route, {
  body: [addCommand.toJSON(), removeCommand.toJSON(), listCommand.toJSON()],
});

async function voxytokRequest(path, options = {}) {
  const response = await fetch(`${process.env.VOXYTOK_URL}${path}`, {
    ...options,
    headers: {
      Authorization: `Bearer ${process.env.VOXYTOK_API_TOKEN}`,
      ...(options.body ? { "Content-Type": "application/json" } : {}),
      ...options.headers,
    },
  });
  if (!response.ok) {
    const body = await response.text();
    throw new Error(`VoxyTok returned HTTP ${response.status}: ${body.slice(0, 300)}`);
  }
  if (response.status === 204) return null;
  return response.json();
}

client.on("interactionCreate", async (interaction) => {
  if (!interaction.isChatInputCommand()) return;

  if (interaction.commandName === "tiktok-add") {
    await interaction.deferReply({ ephemeral: true });
    try {
      const requestedUsername = interaction.options.getString("username", true).trim().replace(/^@/, "");
      const creator = await voxytokRequest("/api/v1/creators", {
        method: "POST",
        body: JSON.stringify({ username: requestedUsername }),
      });
      await pool.query(
        `INSERT INTO voxytok_subscriptions (creator_id, username, added_by)
         VALUES ($1, $2, $3)
         ON CONFLICT (creator_id) DO UPDATE SET username=EXCLUDED.username, added_by=EXCLUDED.added_by`,
        [creator.id, creator.username, interaction.user.id],
      );
      await interaction.editReply(
        `Monitoring enabled for **@${creator.username}**. The first successful fetch creates a baseline; only later publications trigger notifications.`,
      );
    } catch (error) {
      console.error("Could not add TikTok creator:", error);
      await interaction.editReply("Unable to add this profile. Check the username and VoxyTok API status, then try again.");
    }
    return;
  }

  if (interaction.commandName === "tiktok-remove") {
    await interaction.deferReply({ ephemeral: true });
    try {
      const username = interaction.options.getString("username", true).trim().replace(/^@/, "").toLowerCase();
      const result = await pool.query(
        "SELECT creator_id FROM voxytok_subscriptions WHERE username=$1",
        [username],
      );
      if (!result.rowCount) {
        await interaction.editReply(`I don't have a local subscription for **@${username}**.`);
        return;
      }
      await voxytokRequest(`/api/v1/creators/${result.rows[0].creator_id}`, { method: "DELETE" });
      await pool.query("DELETE FROM voxytok_subscriptions WHERE username=$1", [username]);
      await interaction.editReply(`Monitoring stopped for **@${username}**.`);
    } catch (error) {
      console.error("Could not remove TikTok creator:", error);
      await interaction.editReply("Unable to remove this profile. Check the VoxyTok API status and try again.");
    }
    return;
  }

  if (interaction.commandName === "tiktok-list") {
    await interaction.deferReply({ ephemeral: true });
    try {
      const result = await pool.query(
        "SELECT username FROM voxytok_subscriptions ORDER BY username",
      );
      const content = result.rows.length
        ? result.rows.map((row) => `• @${row.username}`).join("\n")
        : "No TikTok profiles are monitored yet. Use /tiktok-add to add one.";
      await interaction.editReply(content.slice(0, 1900));
    } catch (error) {
      console.error("Could not list TikTok creators:", error);
      await interaction.editReply("Unable to load monitored profiles.");
    }
  }
});

// Verify the signature against the exact raw request body before parsing JSON.
// The transaction-level advisory lock prevents concurrent duplicate deliveries
// from both sending a Discord message.
app.post("/voxytok", express.raw({ type: "application/json", limit: "256kb" }), async (req, res) => {
  const key = req.header("Idempotency-Key");
  const signature = req.header("X-VoxyTok-Signature");
  if (!key || !signature?.startsWith("sha256=") || !/^[a-f0-9]{64}$/i.test(signature.slice(7))) {
    return res.status(401).send("Missing or malformed signature/idempotency key");
  }

  const received = Buffer.from(signature.slice(7), "hex");
  const expected = crypto
    .createHmac("sha256", process.env.VOXYTOK_WEBHOOK_SECRET)
    .update(req.body)
    .digest();
  if (received.length !== expected.length || !crypto.timingSafeEqual(received, expected)) {
    return res.status(401).send("Invalid signature");
  }

  const db = await pool.connect();
  try {
    await db.query("BEGIN");
    await db.query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))", [key]);
    const existing = await db.query(
      "SELECT 1 FROM voxytok_webhook_receipts WHERE idempotency_key=$1",
      [key],
    );
    if (existing.rowCount) {
      await db.query("COMMIT");
      return res.sendStatus(204);
    }

    const event = JSON.parse(req.body.toString("utf8"));
    if (event.event === "tiktok.video.published") {
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
    }

    await db.query(
      "INSERT INTO voxytok_webhook_receipts (idempotency_key) VALUES ($1) ON CONFLICT DO NOTHING",
      [key],
    );
    await db.query("COMMIT");
    return res.sendStatus(204);
  } catch (error) {
    await db.query("ROLLBACK").catch(() => {});
    console.error("VoxyTok webhook handling failed:", error);
    return res.status(500).send("Temporary processing failure");
  } finally {
    db.release();
  }
});

app.listen(Number(process.env.PORT || 3000), () => {
  console.log("VoxyTok Discord.js webhook receiver is listening");
});

await client.login(process.env.DISCORD_TOKEN);
