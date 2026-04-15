mod core;
mod hotkey;
mod icons;
mod ui;
mod whisper;

use std::fs::OpenOptions;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    redirect_output_to_log();
    whisper_rs::install_whisper_log_trampoline();
    ui::run()
}

fn redirect_output_to_log() {
    use std::os::unix::io::IntoRawFd;
    let log_path = "/tmp/diktovani.log";
    let Ok(file) = OpenOptions::new().create(true).append(true).open(log_path) else {
        return;
    };
    let fd = file.into_raw_fd();
    unsafe {
        libc::dup2(fd, libc::STDOUT_FILENO);
        libc::dup2(fd, libc::STDERR_FILENO);
        libc::close(fd);
    }
}
