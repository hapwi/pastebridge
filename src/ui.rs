use std::io::IsTerminal;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

fn color(text: &str, code: &str) -> String {
    if std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").as_deref() != Ok("dumb")
    {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn accent(text: &str) -> String {
    color(text, "1;36")
}

pub fn muted(text: &str) -> String {
    color(text, "2")
}

pub fn heading(page: &str) {
    println!();
    println!(
        "  {} {}  {}",
        accent("Pastebridge"),
        muted(env!("CARGO_PKG_VERSION")),
        muted(&format!("/ {page}"))
    );
    println!();
}

pub fn success(message: impl AsRef<str>) {
    println!("  {} {}", color("✓", "32"), message.as_ref());
}

pub fn state(connected: bool) -> String {
    if connected {
        color("● connected", "32")
    } else {
        muted("○ waiting")
    }
}

pub fn hint(message: impl AsRef<str>) {
    println!("  {}", muted(message.as_ref()));
}

pub fn read_line(prompt: &str) -> std::io::Result<Option<String>> {
    use std::io::{self, Write};
    print!("  {prompt} ");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

pub fn select(prompt: &str, options: &[String]) -> anyhow::Result<Option<usize>> {
    for (index, option) in options.iter().enumerate() {
        println!("  {}  {option}", accent(&(index + 1).to_string()));
    }
    println!();
    hint("Enter a number, or press Enter to go back.");
    loop {
        let Some(answer) = read_line(prompt)? else {
            return Ok(None);
        };
        if answer.is_empty() || matches!(answer.as_str(), "q" | "quit") {
            return Ok(None);
        }
        if let Ok(number) = answer.parse::<usize>() {
            if (1..=options.len()).contains(&number) {
                return Ok(Some(number - 1));
            }
        }
        hint(format!("Choose a number from 1 to {}.", options.len()));
    }
}

pub fn spinner(msg: impl Into<String>) -> ProgressBar {
    let msg = msg.into();
    let pb = ProgressBar::new_spinner();
    if !std::io::stderr().is_terminal() {
        pb.set_draw_target(ProgressDrawTarget::hidden());
        eprintln!("  → {msg}");
    }
    pb.set_style(
        ProgressStyle::with_template("  {spinner} {msg}")
            .expect("spinner template")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    pb.set_message(msg);
    pb
}

pub fn stop(spinner: ProgressBar) {
    spinner.finish_and_clear();
}

pub fn confirm(prompt: &str) -> bool {
    read_line(&format!("{prompt} [y/N]"))
        .ok()
        .flatten()
        .is_some_and(|answer| matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
}
