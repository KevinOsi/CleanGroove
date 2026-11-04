//! Prints every stream format each audio device offers, in the order cpal lists them.
//!
//!     cargo run --release -p cr-rt --example device_configs

use cpal::traits::{DeviceTrait, HostTrait};

fn main() -> anyhow::Result<()> {
    let host = cpal::default_host();
    for dev in host.input_devices()? {
        println!("IN  {}", dev.name()?);
        if let Ok(d) = dev.default_input_config() {
            println!("    default: {} ch, {} Hz, {:?}", d.channels(), d.sample_rate().0, d.sample_format());
        }
        for c in dev.supported_input_configs()? {
            println!(
                "    {} ch, {}-{} Hz, {:?}",
                c.channels(),
                c.min_sample_rate().0,
                c.max_sample_rate().0,
                c.sample_format()
            );
        }
    }
    Ok(())
}
