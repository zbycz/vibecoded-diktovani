mod core;
mod ui;

fn main() -> eframe::Result<()> {
    env_logger::init();
    ui::run()
}
