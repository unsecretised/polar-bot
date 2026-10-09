use std::sync::Arc;

pub use poise::serenity_prelude as serenity;

use dotenvy::dotenv;
use poise::serenity_prelude::Mentionable;
use poise::CreateReply;
use reqwest::Client;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateSelectMenu, CreateSelectMenuKind,
};
use serenity::model::application::{ComponentInteraction, ComponentInteractionDataKind};
use serenity::model::Permissions;

use crate::db::{DailyOutcome, DiscountError, DISCOUNT_COST};
use crate::license::{check_is_pro_user, create_discount};
use crate::logging::{COLOR_ERROR, COLOR_INFO, COLOR_SUCCESS};

mod db;
mod license;
mod logging;

const DISCOUNT_COOLDOWN_DAYS: i64 = 30;

pub struct AppData {
    pub http: Client,
    pub db: Arc<db::Db>,
}

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Arc<AppData>, Error>;

fn next_utc_midnight() -> i64 {
    chrono::Utc::now()
        .date_naive()
        .succ_opt()
        .expect("valid date")
        .and_hms_opt(0, 0, 0)
        .expect("valid time")
        .and_utc()
        .timestamp()
}

/// Get the bots version
#[poise::command(slash_command, prefix_command)]
async fn version(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say("v1.1").await?;
    Ok(())
}

/// Get relevant links
#[poise::command(slash_command, prefix_command)]
async fn links(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say(include_str!("../links_response.md")).await?;
    Ok(())
}

fn setup_embed(db: &db::Db) -> CreateEmbed {
    let fmt = |key: &str, kind: char, none: &str| {
        db.get_setting_u64(key)
            .map(|v| format!("<{kind}{v}>"))
            .unwrap_or(none.to_string())
    };
    CreateEmbed::new()
        .title("🔧 Bot Setup")
        .description(format!(
            "**Welcome channel:** {welcome}\n**Logs channel:** {logs}\n**Pro role:** {pro}\n**Free role:** {free}\n\nUse the menus below to configure the bot - changes take effect immediately.",
            welcome = fmt("welcome_channel", '#', "*Not set*"),
            logs = fmt("logs_channel", '#', "*Not set*"),
            pro = fmt("pro_role", '@', "*Not set*"),
            free = fmt("free_role", '@', "*Not set*"),
        ))
        .color(COLOR_INFO)
}

/// Configure the bot (welcome/logs channels and pro/free roles)
#[poise::command(
    slash_command,
    default_member_permissions = "MANAGE_GUILD",
    ephemeral
)]
async fn setup(ctx: Context<'_>) -> Result<(), Error> {
    if ctx.guild_id().is_none() {
        ctx.send(
            CreateReply::default()
                .ephemeral(true)
                .content("This command can only be used inside a server."),
        )
        .await?;
        return Ok(());
    }
    let rows = vec![
        CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                "setup_welcome_channel",
                CreateSelectMenuKind::Channel {
                    channel_types: None,
                    default_channels: None,
                },
            )
            .placeholder("Welcome channel - where new members are greeted"),
        ),
        CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                "setup_logs_channel",
                CreateSelectMenuKind::Channel {
                    channel_types: None,
                    default_channels: None,
                },
            )
            .placeholder("Logs channel - where bot activity is posted"),
        ),
        CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                "setup_pro_role",
                CreateSelectMenuKind::Role {
                    default_roles: None,
                },
            )
            .placeholder("Pro role - given to verified pro users"),
        ),
        CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                "setup_free_role",
                CreateSelectMenuKind::Role {
                    default_roles: None,
                },
            )
            .placeholder("Free role - removed on verification"),
        ),
    ];
    ctx.send(
        CreateReply::default()
            .ephemeral(true)
            .embed(setup_embed(&ctx.data().db))
            .components(rows),
    )
    .await?;
    Ok(())
}

async fn handle_setup_component(
    ctx: &serenity::Context,
    data: &Arc<AppData>,
    ci: &ComponentInteraction,
) -> Result<(), Error> {
    if ci.data.custom_id == "shop_buy_discount" {
        let user_id = ci.user.id.get();
        let outcome = attempt_discount(data, user_id).await;
        ci.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(outcome.message())
                    .ephemeral(true),
            ),
        )
        .await?;
        log_claim_outcome(&ctx.http, data, user_id, &outcome).await;
        return Ok(());
    }

    let Some(key) = ci.data.custom_id.strip_prefix("setup_") else {
        return Ok(());
    };

    let (value, render) = match &ci.data.kind {
        ComponentInteractionDataKind::ChannelSelect { values } => {
            let Some(&v) = values.first() else {
                return Ok(());
            };
            (v.get().to_string(), format!("<#{v}>"))
        }
        ComponentInteractionDataKind::RoleSelect { values } => {
            let Some(&v) = values.first() else {
                return Ok(());
            };
            (v.get().to_string(), format!("<@&{v}>"))
        }
        _ => return Ok(()),
    };

    let Some(member) = &ci.member else {
        return Ok(());
    };
    let is_admin = member
        .permissions
        .is_some_and(|p| p.contains(Permissions::MANAGE_GUILD));
    if !is_admin {
        ci.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("❌ You need the *Manage Server* permission to configure the bot.")
                    .ephemeral(true),
            ),
        )
        .await?;
        return Ok(());
    }

    data.db.set_setting(key, &value)?;
    ci.create_response(
        &ctx.http,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new().embed(setup_embed(&data.db)),
        ),
    )
    .await?;
    logging::log(
        &ctx.http,
        &data.db,
        "Configuration updated",
        format!("{} set **{key}** to {render}", ci.user),
        COLOR_INFO,
    )
    .await;
    Ok(())
}

/// Claim your daily coins (with a streak bonus for consecutive days!)
#[poise::command(slash_command, prefix_command)]
async fn daily(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id.get();
    let today = chrono::Utc::now().date_naive();
    let next_reset = next_utc_midnight();

    match ctx.data().db.daily(user_id as i64, today)? {
        DailyOutcome::AlreadyClaimed { streak, balance } => {
            ctx.send(
                CreateReply::default().ephemeral(true).content(format!(
                    "You already claimed today's coins (balance: **{balance}**). 🔥 Streak: **{streak}** day(s) - next reset <t:{next_reset}:R>"
                )),
            )
            .await?;
        }
        DailyOutcome::Claimed {
            roll,
            bonus,
            streak,
            balance,
        } => {
            let extra = if bonus > 0 {
                format!(" (+{bonus} streak bonus, day {streak} 🔥)")
            } else {
                String::new()
            };
            ctx.send(
                CreateReply::default().ephemeral(true).content(format!(
                    "You claimed **{roll}** coins{extra}! Balance: **{balance}** - come back tomorrow <t:{next_reset}:R>"
                )),
            )
            .await?;
            logging::log(
                ctx.http(),
                &ctx.data().db,
                "Daily coins claimed",
                format!("<@{user_id}> claimed {roll} coins (+{bonus} streak bonus, day {streak})"),
                COLOR_INFO,
            )
            .await;
        }
    }
    Ok(())
}

/// Check your coin balance
#[poise::command(slash_command, prefix_command)]
async fn balance(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id.get();
    let (coins, streak) = ctx.data().db.user_state(user_id as i64)?;
    ctx.send(
        CreateReply::default().ephemeral(true).content(format!(
            "You have **{coins}** coins. 🔥 Current streak: **{streak}** day(s).\nRun `/daily` to earn more - at **{DISCOUNT_COST}** coins you can claim a 25% discount with `/claim_discount`!"
        )),
    )
    .await?;
    Ok(())
}

/// View the top coin holders in the server
#[poise::command(slash_command, prefix_command)]
async fn leaderboard(ctx: Context<'_>) -> Result<(), Error> {
    let rows = ctx.data().db.leaderboard(10)?;

    let description = if rows.is_empty() {
        "No coins have been earned yet - run `/daily` to start earning!".to_string()
    } else {
        rows.into_iter()
            .enumerate()
            .map(|(i, (id, coins))| {
                let rank = match i {
                    0 => "🥇",
                    1 => "🥈",
                    2 => "🥉",
                    _ => "",
                };
                let position = if rank.is_empty() {
                    format!("**{}.**", i + 1)
                } else {
                    rank.to_string()
                };
                format!("{position} <@{id}> - **{coins}** coins")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    ctx.send(
        CreateReply::default().embed(
            CreateEmbed::new()
                .title("🏆 Coin Leaderboard")
                .description(description)
                .color(COLOR_SUCCESS),
        ),
    )
    .await?;
    Ok(())
}

/// See what's available to buy with your coins
#[poise::command(slash_command, prefix_command)]
async fn shop(ctx: Context<'_>) -> Result<(), Error> {
    ctx.send(
        CreateReply::default().embed(
            CreateEmbed::new()
                .title("🛒 Sxitch Shop")
                .description("Spend your hard-earned coins on rewards below!")
                .field(
                    "25% Discount Code",
                    format!(
                        "**Cost:** {DISCOUNT_COST} coins\n**Limit:** 1 per person every {DISCOUNT_COOLDOWN_DAYS} days\n\nCreates a single-use 25% off code you can apply at Polar checkout. Press the button below or use `/claim_discount`."
                    ),
                    false,
                )
                .footer(
                    CreateEmbedFooter::new("Coins are earned with /daily - 15-25 coins + up to 25 streak bonus per day!"),
                )
                .color(COLOR_SUCCESS),
        )
        .components(vec![CreateActionRow::Buttons(vec![
            CreateButton::new("shop_buy_discount")
                .label(format!("Buy - {DISCOUNT_COST} coins"))
                .style(serenity::ButtonStyle::Success),
        ])]),
    )
    .await?;
    Ok(())
}

enum ClaimOutcome {
    InsufficientCoins { balance: i64 },
    OnCooldown { eligible_at: i64 },
    Success { code: String, balance_after: i64 },
    ServerError(String),
}

impl ClaimOutcome {
    fn message(&self) -> String {
        match self {
            ClaimOutcome::InsufficientCoins { balance } => format!(
                "You need **{DISCOUNT_COST}** coins for a discount - you have **{balance}**. Earn more with `/daily`!"
            ),
            ClaimOutcome::OnCooldown { eligible_at } => {
                format!("You can claim your next discount <t:{eligible_at}:R> (<t:{eligible_at}:f>).")
            }
            ClaimOutcome::Success { code, balance_after } => format!(
                "Here's your 25% discount code: **`{code}`**\nApply it at checkout on Polar. {DISCOUNT_COST} coins were deducted - new balance: **{balance_after}**."
            ),
            ClaimOutcome::ServerError(_) => "Creating your discount failed - your coins have been refunded. Please try again later.".to_string(),
        }
    }
}

async fn attempt_discount(data: &AppData, user_id: u64) -> ClaimOutcome {
    let now_ts = chrono::Utc::now().timestamp();

    let tentative = match data.db.try_start_discount(
        user_id as i64,
        DISCOUNT_COST,
        now_ts,
        DISCOUNT_COOLDOWN_DAYS * 86_400,
    ) {
        Ok(tentative) => tentative,
        Err(DiscountError::InsufficientCoins { balance }) => {
            return ClaimOutcome::InsufficientCoins { balance };
        }
        Err(DiscountError::OnCooldown { eligible_at }) => {
            return ClaimOutcome::OnCooldown { eligible_at };
        }
        Err(DiscountError::Db(e)) => return ClaimOutcome::ServerError(e.to_string()),
    };

    match create_discount(&data.http, user_id as i64).await {
        Ok(discount) => {
            match data
                .db
                .finalize_discount(tentative.id, &discount.code, &discount.id)
            {
                Ok(()) => ClaimOutcome::Success {
                    code: discount.code,
                    balance_after: tentative.balance_after,
                },
                Err(e) => ClaimOutcome::ServerError(e.to_string()),
            }
        }
        Err(err) => match data.db.cancel_discount(tentative.id, DISCOUNT_COST) {
            Ok(()) => ClaimOutcome::ServerError(err),
            Err(e) => ClaimOutcome::ServerError(format!("{err}; refund failed: {e}")),
        },
    }
}

async fn log_claim_outcome(http: &serenity::Http, data: &Arc<AppData>, user_id: u64, outcome: &ClaimOutcome) {
    match outcome {
        ClaimOutcome::Success { code, .. } => {
            logging::log(
                http,
                &data.db,
                "Discount claimed",
                format!("<@{user_id}> claimed a 25% discount code `{code}` for {DISCOUNT_COST} coins"),
                COLOR_SUCCESS,
            )
            .await;
        }
        ClaimOutcome::ServerError(err) => {
            logging::log(
                http,
                &data.db,
                "Discount claim failed",
                format!(
                    "<@{user_id}> failed to claim a discount; {DISCOUNT_COST} coins were refunded.\nReason: {err}"
                ),
                COLOR_ERROR,
            )
            .await;
        }
        _ => {}
    }
}

/// Claim a 25% discount code for 200 coins (1 every 30 days)
#[poise::command(slash_command, prefix_command)]
async fn claim_discount(ctx: Context<'_>) -> Result<(), Error> {
    if ctx.guild_id().is_none() {
        ctx.send(
            CreateReply::default()
                .ephemeral(true)
                .content("This command can only be used inside a server."),
        )
        .await?;
        return Ok(());
    }

    let user_id = ctx.author().id.get();
    let outcome = attempt_discount(ctx.data(), user_id).await;
    ctx.send(
        CreateReply::default()
            .ephemeral(true)
            .content(outcome.message()),
    )
    .await?;
    log_claim_outcome(ctx.http(), ctx.data(), user_id, &outcome).await;
    Ok(())
}

/// Check status of your license key (and give you the pro role if valid)
#[poise::command(slash_command, prefix_command)]
async fn check_status(
    ctx: Context<'_>,
    #[description = "Your license key"] license_key: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id.get();
    let Some(mem) = ctx.author_member().await else {
        ctx.send(
            CreateReply::default()
                .ephemeral(true)
                .content("Verification can only be done inside the server."),
        )
        .await?;
        return Ok(());
    };

    if let Some(pro_role) = ctx.data().db.get_setting_u64("pro_role")
        && mem.roles.contains(&serenity::RoleId::new(pro_role))
    {
        ctx.send(
            CreateReply::default()
                .ephemeral(true)
                .content("You're already verified ✅"),
        )
        .await?;
        return Ok(());
    }

    let is_pro = check_is_pro_user(&ctx.data().http, license_key).await;

    if !is_pro {
        ctx.send(
            CreateReply::default()
                .ephemeral(true)
                .content("❌ Couldn't verify that license key. Double-check it and try again."),
        )
        .await?;
        logging::log(
            ctx.http(),
            &ctx.data().db,
            "Verification failed",
            format!("<@{user_id}> failed license verification"),
            COLOR_ERROR,
        )
        .await;
        return Ok(());
    }

    if let Some(role) = ctx.data().db.get_setting_u64("pro_role") {
        mem.add_role(ctx, serenity::RoleId::new(role)).await?;
    }
    if let Some(role) = ctx.data().db.get_setting_u64("free_role") {
        mem.remove_role(ctx, serenity::RoleId::new(role)).await?;
    }

    ctx.send(
        CreateReply::default()
            .ephemeral(true)
            .content("Verified successfully ✅"),
    )
    .await?;
    logging::log(
        ctx.http(),
        &ctx.data().db,
        "User verified",
        format!("<@{user_id}> verified themselves"),
        COLOR_SUCCESS,
    )
    .await;

    Ok(())
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let token = std::env::var("DISCORD_TOKEN").expect("missing DISCORD_TOKEN");
    let db = Arc::new(db::Db::open("sxitchbot.db").expect("failed to open sqlite database"));
    let data = Arc::new(AppData {
        http: Client::new(),
        db,
    });

    let intents = serenity::GatewayIntents::all();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                version(),
                setup(),
                check_status(),
                daily(),
                balance(),
                shop(),
                leaderboard(),
                claim_discount(),
                links(),
            ],
            event_handler: |ctx, event, framework, data| {
                Box::pin(event_handler(ctx, event, framework, data))
            },
            ..Default::default()
        })
        .setup(move |_ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(_ctx, &framework.options().commands).await?;
                Ok(data)
            })
        })
        .build();

    let client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await;
    client.unwrap().start().await.unwrap();
}

async fn event_handler(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    _framework: poise::FrameworkContext<'_, Arc<AppData>, Error>,
    data: &Arc<AppData>,
) -> Result<(), Error> {
    ctx.online();
    match event {
        serenity::FullEvent::GuildMemberAddition { new_member } => {
            let user_id = new_member.user.id.get();
            let channel = data
                .db
                .get_setting_u64("welcome_channel")
                .map(serenity::ChannelId::new)
                .or_else(|| new_member.default_channel(ctx).map(|c| c.id));

            if let Some(channel) = channel {
                channel
                    .say(
                        ctx,
                        format!(
                            "Welcome to the Sxitch Community {}! Run `/check_status` with your license key to verify yourself if you're a pro user, and `/daily` to start earning coins!",
                            new_member.mention()
                        ),
                    )
                    .await
                    .ok();
                logging::log(
                    &ctx.http,
                    &data.db,
                    "Member joined",
                    format!("<@{user_id}> joined and was welcomed in {channel}"),
                    COLOR_INFO,
                )
                .await;
            }
        }
        serenity::FullEvent::InteractionCreate {
            interaction: serenity::Interaction::Component(ci),
        } => {
            handle_setup_component(ctx, data, ci).await?;
        }
        _ => {}
    }
    Ok(())
}
