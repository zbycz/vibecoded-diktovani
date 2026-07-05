mod bubble;
mod core;
mod hotkey;
mod icons;
mod languages;
mod settings;
mod ui;
mod whisper;

use std::fs::OpenOptions;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--check-accessibility") {
        #[cfg(target_os = "macos")]
        // SAFETY: standard AX API call with no invariants to uphold
        unsafe {
            print!("{}", accessibility_sys::AXIsProcessTrusted());
        }
        #[cfg(not(target_os = "macos"))]
        print!("true");
        return Ok(());
    }

    redirect_output_to_log();
    #[cfg(target_os = "macos")]
    core::migrate_launch_agent_identifier("com.example.diktovani");
    whisper_rs::install_whisper_log_trampoline();
    ui::run()
}

fn redirect_output_to_log() {
    use std::os::unix::io::IntoRawFd;
    let log_path = core::LOG_PATH;
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
