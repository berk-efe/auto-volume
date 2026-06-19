use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{process::Command, thread, time::Duration};

#[derive(Deserialize, Debug)]
struct SinkInput {
    index: u32,
    corked: bool,
    mute: bool,
    properties: Properties,
}

#[derive(Deserialize, Debug)]
struct Properties {
    #[serde(rename = "application.name")]
    application_name: Option<String>,

    #[serde(rename = "application.process.binary")]
    application_binary: Option<String>,

    // PipeWire sets this to the browser tab title, e.g. "Song - Artist - YouTube Music"
    #[serde(rename = "media.name")]
    media_name: Option<String>,

    // "event" or "notification" = UI/system sound, never triggers ducking
    #[serde(rename = "media.role")]
    media_role: Option<String>,
}

const DELAY_MILLIS: u64 = 200;
const LOW_VOLUME: u8 = 50;
const STEP: u8 = 5;
// How long to ignore other_audio after the YTM stream index changes (song transition).
// At 200ms/tick, 7 ticks = 1.4 s — enough time for the new stream to stabilise.
const SONG_CHANGE_GRACE: u32 = 7;

fn is_youtube_music(stream: &SinkInput) -> bool {
    let binary = stream.properties.application_binary.as_deref().unwrap_or("").to_lowercase();
    let app_name = stream.properties.application_name.as_deref().unwrap_or("").to_lowercase();
    let media_name = stream.properties.media_name.as_deref().unwrap_or("").to_lowercase();

    // Desktop app variants (youtube-music-desktop-app, youtube-music, etc.)
    if binary.contains("youtube-music") || binary.contains("youtubemusic") {
        return true;
    }

    // Browsers: PipeWire sets media.name to the tab title which includes "YouTube Music".
    // Some Electron builds also register the app name as "YouTube Music".
    app_name.contains("youtube music") || media_name.contains("youtube music")
}

fn is_system_sound(stream: &SinkInput) -> bool {
    let role = stream.properties.media_role.as_deref().unwrap_or("").to_lowercase();
    if role == "event" || role == "notification" || role == "alert" {
        return true;
    }
    // One-shot audio players used for UI sounds
    let binary = stream.properties.application_binary.as_deref().unwrap_or("");
    matches!(binary, "paplay" | "aplay" | "canberra-gtk-play" | "ogg123" | "mpg123")
}

fn main() {
    // Catch SIGTERM (systemd stop) and SIGINT (Ctrl-C) so we can restore volume before exiting.
    // Without this, wireplumber saves whatever reduced volume the script last set as the
    // "preferred" volume for the app, and restores it on every future stream start.
    let shutdown = Arc::new(AtomicBool::new(false));
    ctrlc::set_handler({
        let shutdown = shutdown.clone();
        move || shutdown.store(true, Ordering::SeqCst)
    })
    .expect("Failed to set signal handler");

    let mut current_volume: u8 = 100;
    let mut target_volume: u8 = 100;
    let mut last_ytm_index: Option<u32> = None;
    let mut grace_ticks: u32 = 0;

    loop {
        let json_output = get_pactl_output().expect("Failed to get pactl output");
        let streams: Vec<SinkInput> = parse_json(&json_output);

        let ytm_index = streams.iter()
            .find(|s| !s.corked && is_youtube_music(s))
            .map(|s| s.index);

        if let Some(idx) = ytm_index {
            // New stream index = song just changed. Suppress other_audio briefly so the
            // incoming stream has time to settle before we make any ducking decisions.
            if last_ytm_index != Some(idx) {
                grace_ticks = SONG_CHANGE_GRACE;
                last_ytm_index = Some(idx);
                // New stream — wireplumber may have restored a stale reduced volume.
                // Assert our tracked level immediately so it doesn't stick.
                set_volume(idx, current_volume);
            } else if grace_ticks > 0 {
                grace_ticks -= 1;
            }

            let other_audio = grace_ticks == 0 && streams.iter().any(|s| {
                s.index != idx && !s.corked && !s.mute && !is_system_sound(s)
            });

            target_volume = if other_audio { LOW_VOLUME } else { 100 };

            if current_volume < target_volume {
                current_volume += STEP;
                set_volume(idx, current_volume);
            } else if current_volume > target_volume {
                current_volume -= STEP;
                set_volume(idx, current_volume);
            }
        } else {
            last_ytm_index = None;
            grace_ticks = 0;
        }

        if shutdown.load(Ordering::SeqCst) {
            // Restore YTM to full volume before exiting so wireplumber doesn't
            // save the reduced value as the app's preferred volume.
            if let Some(idx) = last_ytm_index {
                set_volume(idx, 100);
            }
            break;
        }

        thread::sleep(Duration::from_millis(DELAY_MILLIS));
    }
}

fn parse_json(json_string: &str) -> Vec<SinkInput> {
    serde_json::from_str(json_string).expect("Failed to parse JSON")
}

fn set_volume(index: u32, volume_percent: u8) {
    let volume_str = format!("{}%", volume_percent);

    let status = Command::new("pactl")
        .arg("set-sink-input-volume")
        .arg(index.to_string())
        .arg(volume_str)
        .status()
        .expect("Failed to set volume");

    if !status.success() {
        eprintln!("Warning: Failed to set volume on index {}", index);
    } else {
        println!("Set volume to {}% for {}", volume_percent, index);
    }
}

fn get_pactl_output() -> Result<String, String> {
    let output = Command::new("pactl")
        .arg("--format=json")
        .arg("list")
        .arg("sink-inputs")
        .output()
        .expect("Failed to execute pactl");

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(format!("pactl failed: {}", String::from_utf8_lossy(&output.stderr)))
    }
}
