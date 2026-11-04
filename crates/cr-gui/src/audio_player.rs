use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::RingBuffer;

use crate::dsp_bridge::{render_settings, AuditionStream, LinearResampler, RenderSettings, FULL_SCALE};
use crate::state::{AudioTransportControl, AuditionMode, DspTuning};

#[derive(Clone)]
pub struct LoadedAudio {
    pub frames: Arc<Vec<(i32, i32)>>,
    pub sample_rate: u32,
    pub total_frames: usize,
}

pub struct AudioPlayer {
    _stream: Option<cpal::Stream>,
    pub transport: AudioTransportControl,
    pub loaded_audio: Arc<RwLock<Option<LoadedAudio>>>,
    pub tuning: Arc<RwLock<DspTuning>>,
    pub running: Arc<AtomicBool>,
    pub seek_epoch: Arc<AtomicUsize>,
}

impl AudioPlayer {
    pub fn new(transport: AudioTransportControl) -> Self {
        let loaded_audio: Arc<RwLock<Option<LoadedAudio>>> = Arc::new(RwLock::new(None));
        let tuning = Arc::new(RwLock::new(DspTuning::default()));
        let running = Arc::new(AtomicBool::new(true));
        let seek_epoch = Arc::new(AtomicUsize::new(0));

        // Create ring buffer holding (epoch, frame_index, left_sample, right_sample)
        let (mut prod, mut cons) = RingBuffer::<(usize, usize, f32, f32)>::new(8192);

        // CPAL output host and stream initialization
        let host = cpal::default_host();
        let mut device_sample_rate = 48_000u32;
        let mut stream = None;

        let cb_transport = transport.clone();
        let cb_epoch = Arc::clone(&seek_epoch);

        if let Some(device) = host.default_output_device() {
            if let Ok(default_config) = device.default_output_config() {
                device_sample_rate = default_config.sample_rate().0;
                let stream_config = cpal::StreamConfig {
                    channels: 2,
                    sample_rate: cpal::SampleRate(device_sample_rate),
                    buffer_size: cpal::BufferSize::Default,
                };

                let stream_res = device.build_output_stream(
                    &stream_config,
                    move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        let frames_to_fill = output.len() / 2;
                        let active_epoch = cb_epoch.load(Ordering::Relaxed);
                        let mut last_audible_frame = None;

                        for i in 0..frames_to_fill {
                            let mut sample_found = false;
                            while let Ok((ep, f_idx, l, r)) = cons.pop() {
                                if ep == active_epoch {
                                    output[i * 2] = l.clamp(-1.0, 1.0);
                                    output[i * 2 + 1] = r.clamp(-1.0, 1.0);
                                    last_audible_frame = Some(f_idx);
                                    sample_found = true;
                                    break;
                                }
                                // Skip stale frames from previous seek position
                            }

                            if !sample_found {
                                output[i * 2] = 0.0;
                                output[i * 2 + 1] = 0.0;
                            }
                        }

                        // Playhead cursor is locked to the exact audio sample currently being played
                        if let Some(f) = last_audible_frame {
                            cb_transport.playhead_frame.store(f, Ordering::Relaxed);
                        }
                    },
                    |err| {
                        tracing::warn!("Audio output callback error: {}", err);
                    },
                    None,
                );

                if let Ok(s) = stream_res {
                    let _ = s.play();
                    stream = Some(s);
                }
            }
        }

        // DSP & playback generator background worker. The offline engine runs once per source
        // frame at the file's own rate (same engine and settings as the render); only its output
        // is resampled to the device rate.
        let worker_audio = Arc::clone(&loaded_audio);
        let worker_tuning = Arc::clone(&tuning);
        let worker_transport = transport.clone();
        let worker_running = Arc::clone(&running);
        let worker_epoch = Arc::clone(&seek_epoch);
        let has_hardware_stream = stream.is_some();

        thread::spawn(move || {
            let mut engine: Option<AuditionStream> = None;
            let mut engine_params: Option<RenderSettings> = None;
            let mut resampler = LinearResampler::new(48_000, device_sample_rate);
            let mut resampler_rate = 0u32;
            // Next source frame the audition stream will play.
            let mut worker_cur_frame = 0usize;

            while worker_running.load(Ordering::Relaxed) {
                if !worker_transport.is_playing.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(15));
                    continue;
                }

                let audio = match worker_audio.read().unwrap().clone() {
                    Some(a) if a.total_frames > 0 => a,
                    _ => {
                        worker_transport.is_playing.store(false, Ordering::Relaxed);
                        thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                };
                let tune = worker_tuning.read().unwrap().clone();
                let total_frames = audio.total_frames;
                let file_sr = audio.sample_rate.max(1);

                if resampler_rate != file_sr {
                    resampler = LinearResampler::new(file_sr, device_sample_rate.max(1));
                    resampler_rate = file_sr;
                    engine = None;
                }

                // Seek: restart the stream at the playhead.
                if worker_transport.seek_requested.swap(false, Ordering::SeqCst) {
                    worker_epoch.fetch_add(1, Ordering::SeqCst);
                    worker_cur_frame = worker_transport.playhead_frame.load(Ordering::Relaxed).min(total_frames);
                    engine = None;
                }

                // Settings change: rebuild the engine where the stream currently is.
                let params = render_settings(&tune);
                if engine.is_none() || engine_params.as_ref() != Some(&params) {
                    engine = Some(AuditionStream::new(params.clone(), file_sr, Arc::clone(&audio.frames), worker_cur_frame));
                    engine_params = Some(params);
                    resampler.reset();
                }

                // Keep cushion buffer comfortable
                let available_slots = prod.slots();
                if available_slots < 512 {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }

                let batch_size = available_slots.min(1024);
                let loop_start = worker_transport.loop_start_frame.load(Ordering::Relaxed).min(total_frames);
                let loop_end = worker_transport.loop_end_frame.load(Ordering::Relaxed).min(total_frames);
                let is_looping = worker_transport.is_looping.load(Ordering::Relaxed);
                let stop_at = if loop_start < loop_end { loop_end } else { total_frames };
                let cur_epoch = worker_epoch.load(Ordering::Relaxed);
                let mode = tune.audition_mode;

                for _ in 0..batch_size {
                    let eng = engine.as_mut().unwrap();
                    let next = resampler.next(|| {
                        let (idx, (ol, or), (rl, rr), (pl, pr)) = eng.next()?;
                        if idx >= stop_at {
                            return None;
                        }
                        // Out is the full render path (declick and post chain). Noise is what the
                        // declicker removed only, so it is not swamped by the rumble the filters take.
                        let (l, r) = match mode {
                            AuditionMode::OutClean => (pl, pr),
                            AuditionMode::InBypass => (ol as f64, or as f64),
                            AuditionMode::NoiseDelta => ((ol - rl) as f64, (or - rr) as f64),
                        };
                        Some((idx, l as f32 / FULL_SCALE, r as f32 / FULL_SCALE))
                    });

                    match next {
                        Some((idx, l, r)) => {
                            worker_cur_frame = idx + 1;
                            let _ = prod.push((cur_epoch, idx, l, r));
                            // In fallback mode without soundcard, update visible playhead directly
                            if !has_hardware_stream {
                                worker_transport.playhead_frame.store(idx, Ordering::Relaxed);
                            }
                        }
                        None if stop_at < total_frames && is_looping => {
                            worker_cur_frame = loop_start;
                            engine = Some(AuditionStream::new(engine_params.clone().unwrap(), file_sr, Arc::clone(&audio.frames), loop_start));
                            resampler.reset();
                        }
                        None => {
                            worker_transport.is_playing.store(false, Ordering::Relaxed);
                            break;
                        }
                    }
                }

                // Yield briefly if buffer is comfortably cushioned (>4096 frames in flight)
                if prod.slots() < 4096 {
                    thread::sleep(Duration::from_millis(4));
                }
            }
        });

        Self {
            _stream: stream,
            transport,
            loaded_audio,
            tuning,
            running,
            seek_epoch,
        }
    }

    pub fn load_audio(&self, frames: Arc<Vec<(i32, i32)>>, sample_rate: u32) {
        self.transport.stop();
        let total_frames = frames.len();
        {
            let mut guard = self.loaded_audio.write().unwrap();
            *guard = Some(LoadedAudio {
                frames,
                sample_rate,
                total_frames,
            });
        }
        self.seek_epoch.fetch_add(1, Ordering::SeqCst);
        self.transport.seek_to(0);
        self.transport.loop_start_frame.store(0, Ordering::SeqCst);
        self.transport.loop_end_frame.store(0, Ordering::SeqCst);
    }

    /// Stops playback and forgets the loaded audio (File > Close).
    pub fn unload(&self) {
        self.transport.stop();
        *self.loaded_audio.write().unwrap() = None;
        self.seek_epoch.fetch_add(1, Ordering::SeqCst);
        self.transport.seek_to(0);
        self.transport.loop_start_frame.store(0, Ordering::SeqCst);
        self.transport.loop_end_frame.store(0, Ordering::SeqCst);
    }

    pub fn update_tuning(&self, new_tuning: DspTuning) {
        let mut guard = self.tuning.write().unwrap();
        *guard = new_tuning;
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}
