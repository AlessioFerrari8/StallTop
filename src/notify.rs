//! Alert delivery: desktop notification (D-Bus) and terminal bell

use crate::alerts::Alert;
use std::io::Write;

pub struct Notifier {
    pub desktop: bool,
    pub bell: bool,
}

impl Notifier {
    /// Never fails: without D-Bus (ssh, containers) only the bell rings
    pub fn send(&self, alert: &Alert) {
        let (title, body) = alert.message();
        let (title, body) = (escape_markup(&title), escape_markup(&body));
        if self.desktop {
            // D-Bus can be slow or hang; never block the UI on it
            std::thread::spawn(move || {
                let _ = notify_rust::Notification::new()
                    .appname("stalltop")
                    .summary(&title)
                    .body(&body)
                    .show();
            });
        }
        if self.bell {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07");
            let _ = out.flush();
        }
    }
}

/// Notification servers may render the body as markup: `Bash(a && b)` would be malformed or shown wrong, so the three
/// special characters are escaped
pub fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_escaped() {
        assert_eq!(
            escape_markup("Bash(a && b < c > d)"),
            "Bash(a &amp;&amp; b &lt; c &gt; d)"
        );
        assert_eq!(escape_markup("plain"), "plain");
    }
}
