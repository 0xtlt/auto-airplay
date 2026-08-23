use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use airplay_client::{LiveFrameSender, LivePcmFrame};
use anyhow::{Context, Result};

use crate::platform::linux::{OpenedInput, SampleEncoding};

pub struct CaptureHandle {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl CaptureHandle {
    pub fn start(input: OpenedInput, sender: LiveFrameSender) -> Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let thread = thread::Builder::new()
            .name("audio-capture".into())
            .spawn(move || capture_loop(input, sender, thread_running))
            .context("cannot start the audio capture thread")?;

        Ok(Self {
            running,
            thread: Some(thread),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
            && self
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Err(error)) => tracing::warn!(%error, "capture thread stopped with an error"),
                Err(_) => tracing::warn!("capture thread panicked"),
                Ok(Ok(())) => {}
            }
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_loop(
    input: OpenedInput,
    sender: LiveFrameSender,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let channels = input.format.channels as usize;
    let samples_per_period = input.period_frames * channels;

    match input.encoding {
        SampleEncoding::Signed16 => {
            let io = input.pcm.io_i16()?;
            let mut samples = vec![0_i16; samples_per_period];
            while running.load(Ordering::Acquire) {
                match io.readi(&mut samples) {
                    Ok(frames) if frames > 0 => {
                        let length = frames * channels;
                        if !sender.try_send(LivePcmFrame {
                            samples: samples[..length].to_vec(),
                            channels: input.format.channels,
                            sample_rate: input.format.sample_rate,
                        }) {
                            break;
                        }
                    }
                    Ok(_) => continue,
                    Err(error) if error.errno() == libc::EAGAIN => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => input.pcm.try_recover(error, true)?,
                }
            }
        }
        SampleEncoding::Signed32 => {
            let io = input.pcm.io_i32()?;
            let mut raw = vec![0_i32; samples_per_period];
            let mut converted = Vec::with_capacity(samples_per_period);
            while running.load(Ordering::Acquire) {
                match io.readi(&mut raw) {
                    Ok(frames) if frames > 0 => {
                        let length = frames * channels;
                        converted.clear();
                        converted.extend(raw[..length].iter().map(|sample| (sample >> 16) as i16));
                        if !sender.try_send(LivePcmFrame {
                            samples: converted.clone(),
                            channels: input.format.channels,
                            sample_rate: input.format.sample_rate,
                        }) {
                            break;
                        }
                    }
                    Ok(_) => continue,
                    Err(error) if error.errno() == libc::EAGAIN => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => input.pcm.try_recover(error, true)?,
                }
            }
        }
    }

    running.store(false, Ordering::Release);
    Ok(())
}
