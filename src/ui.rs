use crate::core::{LANGUAGE, MODEL_PATH, ModelManager, RecorderState, Result, transcribe_wav_file};
use eframe::egui;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Whispering MVP")
            .with_inner_size([720.0, 420.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Whispering MVP",
        options,
        Box::new(|_cc| Ok(Box::new(WhisperingMvpApp::default()))),
    )
}

enum WorkerEvent {
    TranscriptReady(Result<String>),
}

pub struct WhisperingMvpApp {
    recorder: RecorderState,
    model_manager: ModelManager,
    transcript: String,
    status: String,
    worker_tx: mpsc::Sender<WorkerEvent>,
    worker_rx: mpsc::Receiver<WorkerEvent>,
    is_transcribing: bool,
}

impl Default for WhisperingMvpApp {
    fn default() -> Self {
        let (worker_tx, worker_rx) = mpsc::channel();
        Self {
            recorder: RecorderState::new(),
            model_manager: ModelManager::new(),
            transcript: String::new(),
            status: "Ready. Click start to record from the default microphone.".into(),
            worker_tx,
            worker_rx,
            is_transcribing: false,
        }
    }
}

impl WhisperingMvpApp {
    fn toggle_recording(&mut self) {
        if self.is_transcribing {
            self.status = "Wait for the current transcription to finish.".into();
            return;
        }

        if self.recorder.is_recording() {
            match self.recorder.stop_recording() {
                Ok(recording) => {
                    self.status = format!(
                        "Recording stopped ({:.1}s, {} Hz, {} ch). Transcribing...",
                        recording.duration_seconds, recording.sample_rate, recording.channels
                    );
                    self.is_transcribing = true;
                    self.transcript.clear();

                    let tx = self.worker_tx.clone();
                    let model_manager = self.model_manager.clone();
                    let file_path = recording.file_path;
                    thread::spawn(move || {
                        let result = transcribe_wav_file(&model_manager, &file_path);
                        let _ = tx.send(WorkerEvent::TranscriptReady(result));
                        let _ = std::fs::remove_file(file_path);
                    });
                }
                Err(err) => {
                    self.status = format!("Stop failed: {err}");
                }
            }
            return;
        }

        match self.recorder.start_new_recording() {
            Ok(()) => {
                self.status = "Recording... click the button again to stop and transcribe.".into();
            }
            Err(err) => {
                self.status = format!("Start failed: {err}");
            }
        }
    }
}

impl eframe::App for WhisperingMvpApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.model_manager.unload_if_idle();

        while let Ok(event) = self.worker_rx.try_recv() {
            match event {
                WorkerEvent::TranscriptReady(result) => {
                    self.is_transcribing = false;
                    match result {
                        Ok(transcript) => {
                            println!("{transcript}");
                            self.transcript = transcript;
                            self.status =
                                "Done. Transcript is shown below and printed to stdout.".into();
                        }
                        Err(err) => {
                            self.status = format!("Transcription failed: {err}");
                        }
                    }
                }
            }
        }

        ctx.request_repaint_after(Duration::from_millis(100));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Whispering MVP");
            ui.label("Minimal desktop app extracted from Epicenter Whispering.");
            ui.label(format!("Model: {MODEL_PATH}"));
            ui.label(format!("Language: {LANGUAGE}"));
            ui.add_space(8.0);

            let button_text = if self.recorder.is_recording() {
                "Stop recording"
            } else {
                "Start recording"
            };
            let enabled = !self.is_transcribing;
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new(button_text).min_size([180.0, 44.0].into()),
                )
                .clicked()
            {
                self.toggle_recording();
            }

            ui.add_space(8.0);
            ui.label(format!("Status: {}", self.status));
            ui.add_space(12.0);
            ui.label("Transcript:");
            ui.add(
                egui::TextEdit::multiline(&mut self.transcript)
                    .desired_rows(14)
                    .desired_width(f32::INFINITY),
            );
        });
    }
}
