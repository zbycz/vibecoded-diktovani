mod core;
mod ui;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    ui::run()
}
