use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use std::time::{SystemTime, UNIX_EPOCH};

const APP_ID: &str = "1574411473172561931";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    let mut client = DiscordIpcClient::new(APP_ID);
    client.connect()?;

    client.set_activity(
        activity::Activity::new()
            .details("In the world")
            .state("Desktop")
            .timestamps(activity::Timestamps::new().start(started))
            .assets(
                activity::Assets::new()
                    .large_image("logo")
                    .large_text("Pokem Online"),
            )
            .buttons(vec![activity::Button::new("Play", "https://pokem.onl/")]),
    )?;

    println!("presence published");
    std::thread::sleep(std::time::Duration::from_secs(12));
    Ok(())
}
