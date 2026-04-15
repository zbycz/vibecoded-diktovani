mod core;
mod hotkey;
mod icons;
mod ui;
mod whisper;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    ui::run()
}
