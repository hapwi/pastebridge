use std::io::IsTerminal;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

pub fn spinner(msg: impl Into<String>) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    if !std::io::stderr().is_terminal() {
        pb.set_draw_target(ProgressDrawTarget::hidden());
    }
    pb.set_style(
        ProgressStyle::with_template("{spinner} {msg}")
            .expect("spinner template")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    pb.set_message(msg.into());
    pb
}

pub fn stop(spinner: ProgressBar) {
    spinner.finish_and_clear();
}

pub fn confirm(prompt: &str) -> bool {
    use std::io::{self, Write};
    print!("  {prompt} [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    io::stdin().read_line(&mut line).ok();
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
