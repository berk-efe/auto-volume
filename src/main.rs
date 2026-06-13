use serde::Deserialize;
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

    #[serde(rename = "application.process.id")]
    application_process_id: Option<String>,

    // PipeWire / some PulseAudio setups expose the tab/media title here
    #[serde(rename = "media.name")]
    media_name: Option<String>,

    // Standard PA/PW property: "event" and "notification" are transient system sounds
    #[serde(rename = "media.role")]
    media_role: Option<String>,
}

const DELAY_MILLIS: u64 = 200;
const LOW_VOLUME: u8 = 50;
const STEP: u8 = 5;
// Number of consecutive ticks another stream must be active before ducking kicks in.
// At 200ms/tick this is 1 second — long enough to ignore blips, short enough to feel instant.
const DUCK_DEBOUNCE_TICKS: u32 = 5;

// Substrings matched against the process binary name (lowercased)
const YTM_BINARY_PATTERNS: &[&str] = &["youtube-music", "youtubemusic"];

// One-shot system sound players that should never trigger ducking
const SYSTEM_SOUND_BINARIES: &[&str] = &[
    "paplay",            // pactl / PulseAudio one-shot player
    "aplay",             // ALSA one-shot player
    "canberra-gtk-play", // GTK sound theme player (GNOME, XFCE, …)
    "ogg123",
    "mpg123",
];

const BROWSER_BINARY_PATTERNS: &[&str] = &[
    "chromium", "chrome", "firefox", "brave", "opera", "vivaldi", "edge", "waterfox", "librewolf",
];

fn is_browser_binary(binary: &str) -> bool {
    BROWSER_BINARY_PATTERNS.iter().any(|&b| binary.contains(b))
}

fn is_system_sound(stream: &SinkInput) -> bool {
    let role = stream.properties.media_role.as_deref().unwrap_or("").to_lowercase();
    if role == "event" || role == "notification" {
        return true;
    }
    let binary = stream.properties.application_binary.as_deref().unwrap_or("").to_lowercase();
    SYSTEM_SOUND_BINARIES.iter().any(|&b| binary == b)
}

fn is_youtube_music(stream: &SinkInput) -> bool {
    let binary = stream.properties.application_binary.as_deref().unwrap_or("").to_lowercase();
    let app_name = stream.properties.application_name.as_deref().unwrap_or("").to_lowercase();
    let media_name = stream.properties.media_name.as_deref().unwrap_or("").to_lowercase();

    // Dedicated YouTube Music desktop app (any variant)
    if YTM_BINARY_PATTERNS.iter().any(|&p| binary.contains(p)) {
        return true;
    }

    // Some Electron apps register "YouTube Music" as the application name;
    // PipeWire often sets media.name to the browser tab title
    if app_name.contains("youtube music") || media_name.contains("youtube music") {
        return true;
    }

    // For browsers: check window titles by PID (X11 only, best-effort)
    if is_browser_binary(&binary) {
        if let Some(pid) = &stream.properties.application_process_id {
            return pid_has_youtube_music_window(pid);
        }
    }

    false
}

// Returns true if the process with the given PID has a window titled "YouTube Music".
// Tries wmctrl first (one process call), falls back to xdotool.
// On Wayland or when neither tool is installed, returns false.
fn pid_has_youtube_music_window(pid: &str) -> bool {
    if let Ok(output) = Command::new("wmctrl").args(["-l", "-p"]).output() {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                // Format: 0xWINID  DESKTOP  PID  HOSTNAME  TITLE...
                let mut cols = line.split_whitespace();
                let _wid = cols.next();
                let _desktop = cols.next();
                let line_pid = cols.next().unwrap_or("");
                let _host = cols.next();
                let title = cols.collect::<Vec<_>>().join(" ");
                if line_pid == pid && title.to_lowercase().contains("youtube music") {
                    return true;
                }
            }
            // wmctrl worked but nothing matched — trust it, skip xdotool
            return false;
        }
    }

    // Fallback: xdotool search matching both PID and window name
    if let Ok(output) = Command::new("xdotool")
        .args(["search", "--all", "--pid", pid, "--name", "YouTube Music"])
        .output()
    {
        return !String::from_utf8_lossy(&output.stdout).trim().is_empty();
    }

    false
}

fn main() {
    let mut current_volume: u8 = 100;
    let mut target_volume: u8 = 100;
    let mut duck_ticks: u32 = 0;
    // Indices seen as YTM last tick — lets us immediately apply volume to newly appearing streams.
    let mut prev_ytm_indices: Vec<u32> = Vec::new();

    loop {
        let json_output = get_pactl_output().expect("Failed to get pactl output");
        let streams: Vec<SinkInput> = parse_json(&json_output);

        // Collect ALL active YouTube Music streams.
        // There can be more than one during a song transition (ending + starting stream).
        let ytm_indices: Vec<u32> = streams
            .iter()
            .filter(|s| !s.corked && is_youtube_music(s))
            .map(|s| s.index)
            .collect();

        if !ytm_indices.is_empty() {
            // Binary of the YTM source, used to recognise sibling transition streams.
            let ytm_binary = streams
                .iter()
                .find(|s| ytm_indices.contains(&s.index))
                .and_then(|s| s.properties.application_binary.as_deref())
                .unwrap_or("")
                .to_lowercase();

            let other_audio = streams.iter().any(|s| {
                // Exclude the YTM streams themselves
                if ytm_indices.contains(&s.index) || s.corked || s.mute || is_system_sound(s) {
                    return false;
                }
                // A browser stream from the same binary with no media.name is almost certainly a
                // transition stream for the incoming song — ignore it to avoid false ducking.
                let s_bin = s.properties.application_binary.as_deref().unwrap_or("").to_lowercase();
                if s_bin == ytm_binary && is_browser_binary(&s_bin) {
                    if s.properties.media_name.as_deref().unwrap_or("").is_empty() {
                        return false;
                    }
                }
                true
            });

            if other_audio {
                duck_ticks = duck_ticks.saturating_add(1);
            } else {
                duck_ticks = 0;
            }

            target_volume = if duck_ticks >= DUCK_DEBOUNCE_TICKS { LOW_VOLUME } else { 100 };

            if current_volume < target_volume {
                current_volume += STEP;
            } else if current_volume > target_volume {
                current_volume -= STEP;
            }

            // Apply volume to every YTM stream.
            // New streams (not seen last tick) get the current level immediately so they never
            // start at 100% when we're already ducked.
            for &idx in &ytm_indices {
                if current_volume != target_volume || !prev_ytm_indices.contains(&idx) {
                    set_volume(idx, current_volume);
                }
            }
        } else {
            duck_ticks = 0;
        }

        prev_ytm_indices = ytm_indices;
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
        eprintln!("Warning: Failed to set volume on sink input {}", index);
    } else {
        println!("Set volume to {}% for sink input {}", volume_percent, index);
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
        Err(format!(
            "pactl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}
