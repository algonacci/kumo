use std::fmt::Display;

use anyhow::Error;
use chrono::Local;

fn write(level: &str, component: &str, message: impl Display) {
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let message = message.to_string().replace('\n', " | ");
    eprintln!("[{timestamp}] [{level}] [{component}] {message}");
}

pub fn info(component: &str, message: impl Display) {
    write("INFO", component, message);
}

pub fn warn(component: &str, message: impl Display) {
    write("WARN", component, message);
}

pub fn error(component: &str, context: impl Display, error: &Error) {
    write("ERROR", component, format!("{context} | cause={error:#}"));
}

#[cfg(test)]
mod tests {
    #[test]
    fn multiline_messages_are_kept_on_one_log_line() {
        let message = "first\nsecond".replace('\n', " | ");
        assert_eq!(message, "first | second");
        super::info("test", message);
    }
}
