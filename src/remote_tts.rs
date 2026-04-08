use std::sync::mpsc;
use std::thread;
use std::io::Write;
use rodio::{Decoder, OutputStream, Sink};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSettings {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub voice: String,
    pub speed: f32,
    pub output_device: String,
}

pub enum RemoteTtsCommand {
    Speak(String, RemoteSettings),
    Stop,
}

pub struct RemoteTts {
    cmd_tx: mpsc::Sender<RemoteTtsCommand>,
}

impl RemoteTts {
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<RemoteTtsCommand>();

        thread::spawn(move || {
            let mut current_device_name = String::new();
            let mut stream_info: Option<(OutputStream, rodio::OutputStreamHandle)> = None;

            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::blocking::Client::new());

            // Current sink - we'll recreate it when we need to stop or when device changes
            let mut sink: Option<Sink> = None;

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    RemoteTtsCommand::Speak(text, settings) => {
                        // Check if output device has changed
                        if settings.output_device != current_device_name || stream_info.is_none() {
                            current_device_name = settings.output_device.clone();
                            // Stop current sink because stream is going to change
                            sink = None;
                            
                            use rodio::cpal::traits::{HostTrait, DeviceTrait};
                            let host = rodio::cpal::default_host();
                            let mut target_device = None;
                            
                            if !current_device_name.is_empty() {
                                if let Ok(mut devices) = host.output_devices() {
                                    target_device = devices.find(|d| {
                                        d.name().unwrap_or_default().contains(&current_device_name)
                                    });
                                }
                            }
                            
                            stream_info = target_device
                                .and_then(|d| OutputStream::try_from_device(&d).ok())
                                .or_else(|| {
                                    eprintln!("[RemoteTTS] Falling back to default output device");
                                    OutputStream::try_default().ok()
                                });
                        }

                        // Build the endpoint URL:
                        // If user already typed the full path, use as-is.
                        // Otherwise append /v1/audio/speech
                        let url = {
                            let base = settings.api_url.trim_end_matches('/');
                            if base.ends_with("/v1/audio/speech") {
                                base.to_string()
                            } else if base.ends_with("/v1") {
                                format!("{base}/audio/speech")
                            } else {
                                format!("{base}/v1/audio/speech")
                            }
                        };

                        // Build request body - minimal, matching the Python reference
                        let body = serde_json::json!({
                            "model": settings.model,
                            "input": text,
                            "voice": settings.voice,
                        });

                        // API key: use "none" if empty (some self-hosted services accept this)
                        let api_key = if settings.api_key.trim().is_empty() {
                            "none".to_string()
                        } else {
                            settings.api_key.clone()
                        };

                        eprintln!("[RemoteTTS] POST {url}  voice={}", settings.voice);

                        let response = client
                            .post(&url)
                            .header("Authorization", format!("Bearer {api_key}"))
                            .header("Content-Type", "application/json")
                            .json(&body)
                            .send();

                        match response {
                            Ok(res) => {
                                let status = res.status();
                                if !status.is_success() {
                                    let err_body = res.text().unwrap_or_default();
                                    eprintln!("[RemoteTTS] Server returned {status}: {err_body}");
                                    continue;
                                }

                                match res.bytes() {
                                    Ok(bytes) => {
                                        if bytes.is_empty() {
                                            eprintln!("[RemoteTTS] Server returned empty audio");
                                            continue;
                                        }

                                        // Decode directly from memory to avoid file locks when queueing
                                        let cursor = std::io::Cursor::new(bytes.to_vec());
                                        match Decoder::new(cursor) {
                                            Ok(source) => {
                                                // If we don't have a sink or it has been stopped, create a new one
                                                if sink.is_none() {
                                                    if let Some((_, handle)) = &stream_info {
                                                        match Sink::try_new(handle) {
                                                            Ok(new_sink) => {
                                                                sink = Some(new_sink);
                                                            }
                                                            Err(e) => {
                                                                eprintln!("[RemoteTTS] Failed to create sink: {e}");
                                                                continue;
                                                            }
                                                        }
                                                    } else {
                                                        eprintln!("[RemoteTTS] No audio stream available");
                                                        continue;
                                                    }
                                                }

                                                // Append to the queue
                                                if let Some(s) = &sink {
                                                    s.append(source);
                                                    eprintln!("[RemoteTTS] Queued audio ({} bytes)", bytes.len());
                                                }
                                            }
                                            Err(e) => {
                                                eprintln!("[RemoteTTS] Failed to decode audio: {e}");
                                                let preview: Vec<u8> = bytes.iter().take(16).copied().collect();
                                                eprintln!("[RemoteTTS] First bytes: {:?}", preview);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("[RemoteTTS] Failed to read response body: {e}");
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("[RemoteTTS] Request failed: {e}");
                            }
                        }
                    }
                    RemoteTtsCommand::Stop => {
                        if let Some(old_sink) = sink.take() {
                            old_sink.stop();
                        }
                    }
                }
            }
        });

        Self { cmd_tx }
    }

    pub fn send(&self, cmd: RemoteTtsCommand) {
        let _ = self.cmd_tx.send(cmd);
    }
}
