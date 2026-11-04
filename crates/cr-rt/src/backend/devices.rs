//! Audio device scan and selection.
//!
//! The GUI lists every input and output with the standard sample rates it supports, and picks
//! a device by its exact name. The CLI may still pass part of a name ("USB").
//!
//! On Linux, ALSA offers dozens of routes per card (dmix, dsnoop, surround51, speex, upmix...).
//! The list keeps the ones a recordist wants: the system default, the sound server (PipeWire,
//! else PulseAudio) and each card's direct `hw:` device, labelled with the card's own name.
//! Any other ALSA name still works when typed (CLI).

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait};

/// Sample rates offered to the user, in the order shown.
pub const STANDARD_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    /// The name the device is opened by.
    pub name: String,
    /// What to show the user.
    pub label: String,
    /// False when the device could not be queried, usually because another program holds it
    /// exclusively (an ALSA `hw:` device in use). It cannot be started until freed.
    pub available: bool,
    pub is_default: bool,
    /// Standard rates (from `STANDARD_RATES`) the device reports support for.
    pub sample_rates: Vec<u32>,
    /// The device's own default rate (on WASAPI: the rate set in the Windows Sound settings).
    pub default_rate: Option<u32>,
    pub max_channels: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceScan {
    pub host: String,
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
}

/// Rates both devices support, in `STANDARD_RATES` order.
pub fn common_rates(input: &DeviceInfo, output: &DeviceInfo) -> Vec<u32> {
    input
        .sample_rates
        .iter()
        .copied()
        .filter(|r| output.sample_rates.contains(r))
        .collect()
}

pub fn scan_devices() -> Result<DeviceScan> {
    quiet_stderr(|| {
        let host = cpal::default_host();
        let default_in = host.default_input_device().and_then(|d| d.name().ok());
        let default_out = host.default_output_device().and_then(|d| d.name().ok());
        let cards = system_cards();

        let mut lists = [Vec::new(), Vec::new()];
        for (k, is_input) in [(0, true), (1, false)] {
            let devices: Vec<cpal::Device> = if is_input {
                host.input_devices()?.collect()
            } else {
                host.output_devices()?.collect()
            };
            let names: Vec<String> = devices.iter().map(|d| d.name().unwrap_or_default()).collect();
            let default = if is_input { &default_in } else { &default_out };
            for (dev, name) in devices.iter().zip(&names) {
                if !keep_device(name, &names) {
                    continue;
                }
                if let Some(mut info) = describe(dev, is_input, default.as_deref()) {
                    info.label = device_label(name, &cards);
                    lists[k].push(info);
                }
            }
        }
        let [inputs, outputs] = lists;
        Ok(DeviceScan {
            host: format!("{:?}", host.id()),
            inputs,
            outputs,
        })
    })
}

/// ALSA card id -> (card description, device number -> PCM name), from /proc/asound.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SystemCards {
    pub cards: Vec<(String, String)>,
    pub pcms: Vec<(String, u32, String)>,
}

fn system_cards() -> SystemCards {
    if !cfg!(target_os = "linux") {
        return SystemCards::default();
    }
    parse_asound(
        &std::fs::read_to_string("/proc/asound/cards").unwrap_or_default(),
        &std::fs::read_to_string("/proc/asound/pcm").unwrap_or_default(),
    )
}

/// Parses /proc/asound/cards (" 1 [CODEC   ]: USB-Audio - USB AUDIO  CODEC") and
/// /proc/asound/pcm ("01-00: USB Audio : USB Audio : playback 1 : capture 1").
pub fn parse_asound(cards: &str, pcm: &str) -> SystemCards {
    let mut out = SystemCards::default();
    let mut index_to_id = Vec::new();
    for line in cards.lines() {
        let (Some(open), Some(close)) = (line.find('['), line.find(']')) else { continue };
        let Ok(index) = line[..open].trim().parse::<u32>() else { continue };
        let id = line[open + 1..close].trim().to_string();
        let desc = line[close + 1..]
            .split_once(" - ")
            .map(|(_, d)| d.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| id.clone());
        index_to_id.push((index, id.clone()));
        out.cards.push((id, desc));
    }
    for line in pcm.lines() {
        let mut parts = line.split(':').map(str::trim);
        let (Some(addr), Some(_), Some(name)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Some((c, d)) = addr.split_once('-') else { continue };
        let (Ok(c), Ok(d)) = (c.parse::<u32>(), d.parse::<u32>()) else { continue };
        if let Some((_, id)) = index_to_id.iter().find(|(i, _)| *i == c) {
            out.pcms.push((id.clone(), d, name.to_string()));
        }
    }
    out
}

/// Which devices to list. Everywhere but Linux: all. On Linux (ALSA names): the default, the
/// sound server (PipeWire if present, else PulseAudio) and each card's direct `hw:` device.
pub fn keep_device(name: &str, all_names: &[String]) -> bool {
    if !all_names.iter().any(|n| n.starts_with("hw:CARD=")) {
        return true; // not ALSA
    }
    match name {
        "default" | "pipewire" => true,
        "pulse" => !all_names.iter().any(|n| n == "pipewire"),
        n => n.starts_with("hw:CARD="),
    }
}

/// A readable name: "USB AUDIO CODEC - USB Audio (direct)" for `hw:CARD=CODEC,DEV=0`.
pub fn device_label(name: &str, sys: &SystemCards) -> String {
    match name {
        "default" => return "System default (shared)".to_string(),
        "pipewire" => return "PipeWire (shared)".to_string(),
        "pulse" => return "PulseAudio (shared)".to_string(),
        _ => {}
    }
    let Some(rest) = name.strip_prefix("hw:CARD=") else { return name.to_string() };
    let (card, dev) = match rest.split_once(",DEV=") {
        Some((c, d)) => (c, d.parse::<u32>().unwrap_or(0)),
        None => (rest, 0),
    };
    let Some((_, desc)) = sys.cards.iter().find(|(id, _)| id == card) else { return name.to_string() };
    match sys.pcms.iter().find(|(id, d, _)| id == card && *d == dev) {
        Some((_, _, pcm)) => format!("{} - {} (direct)", desc, pcm),
        None => format!("{}, device {} (direct)", desc, dev),
    }
}

/// While cpal probes devices, ALSA plugins (dmix, dsnoop, OSS, JACK) print complaints straight
/// to stderr: "unable to open slave", "supports only playback stream", "jack server is not
/// running". None is an error of ours; the ones that matter come back as `Result`s. This runs
/// `f` with stderr sent to /dev/null and restores it afterwards, also on panic.
#[cfg(unix)]
pub(crate) fn quiet_stderr<T>(f: impl FnOnce() -> T) -> T {
    use std::os::fd::AsRawFd;

    struct Restore(i32);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                libc::dup2(self.0, 2);
                libc::close(self.0);
            }
        }
    }

    let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") else { return f() };
    let saved = unsafe { libc::dup(2) };
    if saved < 0 {
        return f();
    }
    let _restore = Restore(saved);
    unsafe { libc::dup2(null.as_raw_fd(), 2) };
    f()
}

#[cfg(not(unix))]
pub(crate) fn quiet_stderr<T>(f: impl FnOnce() -> T) -> T {
    f()
}

fn describe(dev: &cpal::Device, is_input: bool, default_name: Option<&str>) -> Option<DeviceInfo> {
    let name = dev.name().ok()?;
    let configs: Vec<_> = match if is_input {
        dev.supported_input_configs().map(|c| c.collect())
    } else {
        dev.supported_output_configs().map(|c| c.collect())
    } {
        Ok(c) => c,
        // Listed but cannot be queried: usually held exclusively by another program.
        Err(_) => {
            return Some(DeviceInfo {
                is_default: default_name == Some(name.as_str()),
                label: name.clone(),
                name,
                available: false,
                sample_rates: Vec::new(),
                default_rate: None,
                max_channels: 0,
            })
        }
    };
    let default_rate = if is_input {
        dev.default_input_config().ok().map(|c| c.sample_rate().0)
    } else {
        dev.default_output_config().ok().map(|c| c.sample_rate().0)
    };
    let sample_rates = STANDARD_RATES
        .iter()
        .copied()
        .filter(|&r| {
            configs
                .iter()
                .any(|c| c.min_sample_rate().0 <= r && c.max_sample_rate().0 >= r)
        })
        .collect();
    let max_channels = configs.iter().map(|c| c.channels()).max().unwrap_or(0);
    Some(DeviceInfo {
        is_default: default_name == Some(name.as_str()),
        label: name.clone(),
        name,
        available: true,
        sample_rates,
        default_rate,
        max_channels,
    })
}

/// Prints the scan for the CLI's `--list-devices`.
pub fn list_devices() -> Result<()> {
    let scan = scan_devices()?;
    println!("Audio Host: {}", scan.host);
    for (title, list) in [("Input", &scan.inputs), ("Output", &scan.outputs)] {
        println!("\n=== Available Audio {} Devices ===", title);
        for (idx, d) in list.iter().enumerate() {
            let rates: Vec<String> = d.sample_rates.iter().map(|r| format!("{}", r)).collect();
            let what = if !d.available {
                "busy or unavailable".to_string()
            } else if rates.is_empty() {
                format!("{} ch; no standard rate", d.max_channels)
            } else {
                format!("{} ch; {}", d.max_channels, rates.join(", "))
            };
            let name = if d.label != d.name { format!("{}  [{}]", d.label, d.name) } else { d.name.clone() };
            println!(" [{}] {}{}  ({})", idx, name, if d.is_default { " (default)" } else { "" }, what);
        }
    }
    println!("\nPass the name in [brackets] (or a unique part of it) to --input / --output.");
    println!();
    Ok(())
}

/// Picks a name from `names`: an exact match wins; otherwise a case-insensitive part of a name
/// must match exactly one device.
pub fn select_by_name(names: &[String], wanted: &str) -> Result<usize> {
    if let Some(i) = names.iter().position(|n| n == wanted) {
        return Ok(i);
    }
    let w = wanted.to_lowercase();
    let hits: Vec<usize> = (0..names.len())
        .filter(|&i| names[i].to_lowercase().contains(&w))
        .collect();
    match hits.as_slice() {
        [i] => Ok(*i),
        [] => Err(anyhow!("Audio device '{}' not found", wanted)),
        _ => Err(anyhow!(
            "'{}' matches several audio devices: {}",
            wanted,
            hits.iter().map(|&i| names[i].as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

pub(crate) fn find_device(is_input: bool, wanted: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let Some(wanted) = wanted else {
        let dev = if is_input {
            host.default_input_device()
        } else {
            host.default_output_device()
        };
        return dev.ok_or_else(|| {
            anyhow!("No default {} audio device found", if is_input { "input" } else { "output" })
        });
    };
    // Finding it probes every device, as the scan does; the stream itself is opened (and any
    // real error reported) by the caller, outside this.
    quiet_stderr(|| {
        let mut devices: Vec<cpal::Device> = if is_input {
            host.input_devices()?.collect()
        } else {
            host.output_devices()?.collect()
        };
        let names: Vec<String> = devices
            .iter()
            .map(|d| d.name().unwrap_or_default())
            .collect();
        let idx = select_by_name(&names, wanted)?;
        Ok(devices.swap_remove(idx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exact_name_beats_partial_match() {
        let n = names(&["Line In (USB Audio CODEC)", "Line In (USB Audio CODEC) 2", "Microphone"]);
        assert_eq!(select_by_name(&n, "Line In (USB Audio CODEC)").unwrap(), 0);
        assert_eq!(select_by_name(&n, "Line In (USB Audio CODEC) 2").unwrap(), 1);
    }

    #[test]
    fn partial_name_must_be_unique() {
        let n = names(&["Line In (USB Audio CODEC)", "Speakers (Realtek)", "Microphone"]);
        assert_eq!(select_by_name(&n, "realtek").unwrap(), 1);
        assert!(select_by_name(&n, "e").is_err());
        assert!(select_by_name(&n, "Focusrite").is_err());
    }

    #[test]
    fn common_rates_keeps_order() {
        let a = DeviceInfo {
            name: "a".into(),
            label: "a".into(),
            available: true,
            is_default: false,
            sample_rates: vec![44_100, 48_000, 96_000],
            default_rate: Some(48_000),
            max_channels: 2,
        };
        let b = DeviceInfo {
            sample_rates: vec![48_000, 96_000, 192_000],
            ..a.clone()
        };
        assert_eq!(common_rates(&a, &b), vec![48_000, 96_000]);
    }

    /// The device names ALSA offered on the MX Linux test box (2026-10-30).
    const MX_INPUTS: &[&str] = &[
        "lavrate", "samplerate", "speexrate", "pipewire", "pulse", "speex", "upmix", "vdownmix",
        "default", "hw:CARD=PCH,DEV=0", "plughw:CARD=PCH,DEV=0", "sysdefault:CARD=PCH",
        "front:CARD=PCH,DEV=0", "surround40:CARD=PCH,DEV=0", "dsnoop:CARD=PCH,DEV=0",
        "hw:CARD=CODEC,DEV=0", "plughw:CARD=CODEC,DEV=0", "sysdefault:CARD=CODEC",
        "front:CARD=CODEC,DEV=0", "iec958:CARD=CODEC,DEV=0", "dsnoop:CARD=CODEC,DEV=0",
    ];

    const MX_CARDS: &str = " 0 [PCH            ]: HDA-Intel - HDA Intel PCH
                      HDA Intel PCH at 0xa1310000 irq 133
 1 [CODEC          ]: USB-Audio - USB AUDIO  CODEC
                      BurrBrown from Texas Instruments USB AUDIO  CODEC at usb-0000:00:15.0-1, full s
";
    const MX_PCM: &str = "00-00: ALC269VC Analog : ALC269VC Analog : playback 1 : capture 1
00-03: HDMI 0 : HDMI 0 : playback 1
01-00: USB Audio : USB Audio : playback 1 : capture 1
";

    #[test]
    fn linux_list_keeps_default_server_and_direct_devices() {
        let all = names(MX_INPUTS);
        let kept: Vec<&str> = all.iter().filter(|n| keep_device(n, &all)).map(|s| s.as_str()).collect();
        assert_eq!(kept, ["pipewire", "default", "hw:CARD=PCH,DEV=0", "hw:CARD=CODEC,DEV=0"]);

        // Without PipeWire, PulseAudio stands in for the sound server.
        let no_pw: Vec<String> = all.iter().filter(|n| *n != "pipewire").cloned().collect();
        assert!(keep_device("pulse", &no_pw));

        // Not ALSA (WASAPI, CoreAudio): everything stays.
        let win = names(&["Microphone (USB Audio CODEC )", "Speakers (Realtek(R) Audio)"]);
        assert!(win.iter().all(|n| keep_device(n, &win)));
    }

    #[test]
    fn linux_labels_name_the_card() {
        let sys = parse_asound(MX_CARDS, MX_PCM);
        assert_eq!(device_label("hw:CARD=CODEC,DEV=0", &sys), "USB AUDIO CODEC - USB Audio (direct)");
        assert_eq!(device_label("hw:CARD=PCH,DEV=0", &sys), "HDA Intel PCH - ALC269VC Analog (direct)");
        assert_eq!(device_label("hw:CARD=PCH,DEV=3", &sys), "HDA Intel PCH - HDMI 0 (direct)");
        assert_eq!(device_label("hw:CARD=PCH,DEV=9", &sys), "HDA Intel PCH, device 9 (direct)");
        assert_eq!(device_label("pipewire", &sys), "PipeWire (shared)");
        assert_eq!(device_label("hw:CARD=GONE,DEV=0", &sys), "hw:CARD=GONE,DEV=0");
        assert_eq!(device_label("Speakers (Realtek(R) Audio)", &sys), "Speakers (Realtek(R) Audio)");
    }
}
