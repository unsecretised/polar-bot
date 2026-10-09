use poise::serenity_prelude as serenity;
use serenity::builder::{CreateEmbed, CreateMessage};
use serenity::model::timestamp::Timestamp;

use crate::db::Db;

pub const COLOR_INFO: u32 = 0x5865F2;
pub const COLOR_SUCCESS: u32 = 0x57F287;
pub const COLOR_ERROR: u32 = 0xED4245;

pub async fn log(
    http: &serenity::Http,
    db: &Db,
    title: &str,
    description: impl Into<String>,
    color: u32,
) {
    let description = description.into();
    tracing::info!(target: "bot_log", "{} - {}", title, description);

    let Some(channel_id) = db.get_setting_u64("logs_channel") else {
        return;
    };

    let embed = CreateEmbed::new()
        .title(title)
        .description(&description)
        .color(color)
        .timestamp(Timestamp::now());

    if let Err(err) = serenity::ChannelId::new(channel_id)
        .send_message(http, CreateMessage::new().embed(embed))
        .await
    {
        tracing::warn!(%err, "failed to send log embed");
    }
}
