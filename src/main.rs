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
    application_name: String,

    #[serde(rename = "application.process.binary")]
    application_binary: String,
}

fn main() {
    loop {
        // get the output string of pactl
        let json_output = get_pactl_output().expect("huh");
        let streams: Vec<SinkInput> = parse_json(&json_output);

        let mut music_app_index: Option<u32> = None;

        for stream in &streams {
            if stream.properties.application_binary == "youtube-music-desktop-app"
                && stream.properties.application_name == "Chromium"
                && !stream.corked
            {
                music_app_index = Some(stream.index);
            }
        }

        // the music app is up and running
        if let Some(music_index) = music_app_index {
            let mut running_apps: bool = false;
            for stream in streams {
                if stream.index != music_index {
                    if !stream.corked {
                        if !stream.mute {
                            running_apps = true;
                        }
                    }
                }
            }

            if !running_apps {
                set_volume(music_index, 100);
            } else {
                set_volume(music_index, 35);
            }
        }

        thread::sleep(Duration::from_millis(200));
    }
}

fn parse_json(json_string: &str) -> Vec<SinkInput> {
    let streams: Vec<SinkInput> = serde_json::from_str(json_string).expect("Failed to parse JSON");
    return streams;
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
    }
}

fn get_pactl_output() -> Result<String, String> {
    let output = Command::new("pactl")
        .arg("--format=json")
        .arg("list")
        .arg("sink-inputs")
        .output()
        .expect("Failed to execute pactl command");

    if output.status.success() {
        // output.stdout is a Vec<u8> (a vector of bytes).
        // we convert it to a String.
        let output_string = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(output_string)
    } else {
        let error_string = String::from_utf8_lossy(&output.stderr).to_string();
        Err(format!("pactl command failed: {}", error_string))
    }
}
