use std::fs;

pub use poise::serenity_prelude as serenity;

use dotenvy::dotenv;
use poise::{
    CreateReply,
    serenity_prelude::{ChannelId, Error, Mentionable, RoleId, futures::lock::Mutex},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::license::check_is_pro_user;

mod license;

#[derive(Clone)]
pub struct AppData {
    welcome_channel: Option<ChannelId>,
    pro_role: Option<RoleId>,
    free_role: Option<RoleId>,
    client: reqwest::Client,
}

impl From<AppData> for StorableAppData {
    fn from(value: AppData) -> Self {
        StorableAppData {
            welcome_channel: value.welcome_channel.map(|x| x.get()),
            pro_role: value.pro_role.map(|x| x.get()),
            free_role: value.free_role.map(|x| x.get()),
        }
    }
}

impl From<StorableAppData> for AppData {
    fn from(value: StorableAppData) -> Self {
        AppData {
            welcome_channel: value.welcome_channel.map(|x| ChannelId::new(x)),
            pro_role: value.pro_role.map(|x| RoleId::new(x)),
            free_role: value.free_role.map(|x| RoleId::new(x)),
            client: Client::new(),
        }
    }
}

#[derive(Deserialize, Serialize, Default)]
pub struct StorableAppData {
    welcome_channel: Option<u64>,
    pro_role: Option<u64>,
    free_role: Option<u64>,
}

impl StorableAppData {
    fn load() -> Option<StorableAppData> {
        fs::read_to_string("instance.json")
            .ok()
            .and_then(|x| serde_json::from_str(&x).ok())
    }

    fn save(&self) -> Option<()> {
        serde_json::to_string_pretty(&self)
            .ok()
            .and_then(|x| fs::write("instance.json", x).ok())
    }
}

type Context<'a> = poise::Context<'a, Mutex<AppData>, Error>;

/// Get the bots version
#[poise::command(slash_command, prefix_command)]
async fn version(ctx: Context<'_>) -> Result<(), Error> {
    let response = "v1.0";
    ctx.say(response).await?;
    Ok(())
}

/// Get relevant links
#[poise::command(slash_command, prefix_command)]
async fn links(ctx: Context<'_>) -> Result<(), Error> {
    let response = include_str!("../links_response.md");
    ctx.say(response).await?;
    Ok(())
}

/// Set the greeting channel
#[poise::command(slash_command, prefix_command, required_permissions = "MANAGE_GUILD")]
async fn set_general(
    ctx: Context<'_>,
    #[description = "The channel to check"]
    #[channel_types("Text")] // Restrict which channel types are allowed
    channel: serenity::Channel,
) -> Result<(), Error> {
    ctx.data().lock().await.welcome_channel = Some(channel.id());
    StorableAppData::from(ctx.data().lock().await.clone()).save();
    ctx.send(
        CreateReply::default()
            .ephemeral(true)
            .content(channel.to_string() + " was set as the default channel for welcome messages."),
    )
    .await?;
    Ok(())
}

/// Set the role to give pro users
#[poise::command(slash_command, prefix_command, required_permissions = "MANAGE_GUILD")]
async fn set_pro_role(
    ctx: Context<'_>,
    #[description = "The role to assign pro users"] role: serenity::Role,
) -> Result<(), Error> {
    let role_name = role.to_string();
    ctx.data().lock().await.pro_role = Some(role.id);
    StorableAppData::from(ctx.data().lock().await.clone()).save();
    ctx.send(
        CreateReply::default()
            .ephemeral(true)
            .content(role_name + " was set as the role to assign pro users."),
    )
    .await?;
    Ok(())
}

/// Set the free role to remove from users
#[poise::command(slash_command, prefix_command, required_permissions = "MANAGE_GUILD")]
async fn set_free_role(
    ctx: Context<'_>,
    #[description = "The role to assign free users"] role: serenity::Role,
) -> Result<(), Error> {
    let role_name = role.to_string();
    ctx.data().lock().await.free_role = Some(role.id);
    StorableAppData::from(ctx.data().lock().await.clone()).save();
    ctx.send(
        CreateReply::default()
            .ephemeral(true)
            .content(role_name + " was set as the role to assign free users."),
    )
    .await?;
    Ok(())
}

/// Check status of your license key (and give you the pro role if valid)
#[poise::command(slash_command, prefix_command)]
async fn check_status(
    ctx: Context<'_>,
    #[description = "Your license key"] license_key: String,
) -> Result<(), Error> {
    let data = ctx.data().lock().await;

    let Some(mem) = ctx.author_member().await else {
        return Ok(());
    };

    if let Some(pro_role) = &data.pro_role
        && mem.roles.contains(&pro_role)
    {
        CreateReply::default()
            .ephemeral(true)
            .content("Verified Already");
        return Ok(());
    }

    let is_pro = check_is_pro_user(&data.client, license_key).await;

    if !is_pro {
        ctx.send(
            CreateReply::default()
                .ephemeral(true)
                .content("Couldn't verify user"),
        )
        .await?;

        return Ok(());
    }

    if let Some(role) = &data.pro_role {
        mem.add_role(ctx, role).await?;
    }

    if let Some(role) = &data.free_role {
        mem.remove_role(ctx, role).await?;
    }

    ctx.send(
        CreateReply::default()
            .ephemeral(true)
            .content("Verified user successfully"),
    )
    .await?;

    Ok(())
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    let token = std::env::var("DISCORD_TOKEN").expect("missing DISCORD_TOKEN");
    let intents = serenity::GatewayIntents::all();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                version(),
                set_general(),
                set_free_role(),
                set_pro_role(),
                check_status(),
                links(),
            ],
            event_handler: |ctx, event, framework, data| {
                Box::pin(event_handler(ctx, event, framework, data))
            },
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                Ok(Mutex::new(AppData::from(
                    StorableAppData::load().unwrap_or_default(),
                )))
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
    _framework: poise::FrameworkContext<'_, Mutex<AppData>, Error>,
    data: &Mutex<AppData>,
) -> Result<(), Error> {
    ctx.online();
    match event {
        serenity::FullEvent::GuildMemberAddition { new_member } => {
            let data = data.lock().await;
            let default_channel = new_member.default_channel(ctx).unwrap();
            let channel = data.welcome_channel.as_ref().unwrap_or(&default_channel.id);

            channel
                .say(
                    ctx,
                    format!("Welcome to the Sxitch Community {}! Run `/check_status` with you're license key to verify yourself if you're a pro user!", new_member.mention()),
                )
                .await
                .ok();
        }
        _ => {}
    }
    Ok(())
}
