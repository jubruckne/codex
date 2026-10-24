//! Native-only invocation details. Results remain in the existing full-transcript renderer.
use crate::history_cell::activity_preview::clipped_line;
use crate::motion::MotionMode;
use crate::motion::loading_glyph;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::tui::FrameRequester;
use ratatui::style::Stylize;
use ratatui::text::Line;
use serde_json::Value;
use std::time::Instant;

// Bound ordinary argument previews before serialization; never mutate retained arguments.
fn safe_argument(value: &Value, depth: usize) -> Value {
    if depth > 8 {
        return Value::String("…".into());
    }
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .take(32)
                .map(|(key, value)| {
                    let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
                    let secret = [
                        "token",
                        "password",
                        "secret",
                        "credential",
                        "apikey",
                        "accesskey",
                        "privatekey",
                        "authheader",
                        "bearer",
                        "passwd",
                        "authorization",
                        "cookie",
                        "headers",
                    ]
                    .iter()
                    .any(|part| normalized.contains(part));
                    (
                        key.clone(),
                        if secret {
                            Value::String("<redacted>".into())
                        } else {
                            safe_argument(value, depth + 1)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(32)
                .map(|item| safe_argument(item, depth + 1))
                .collect(),
        ),
        Value::String(text) if text.len() > 2048 => {
            Value::String(format!("{}…", &text[..text.floor_char_boundary(2048)]))
        }
        _ => value.clone(),
    }
}

pub(super) fn arguments(value: &Value, width: u16) -> Vec<HyperlinkLine> {
    let safe = safe_argument(value, 0);
    let text = safe.to_string();
    let text = &text[..text.floor_char_boundary(4096)];
    vec![clipped_line(Line::from(format!("  └ {text}")).dim(), width)]
}

pub(super) fn spinner(
    start: Instant,
    animations: bool,
    requester: Option<&FrameRequester>,
    width: u16,
) -> HyperlinkLine {
    let glyph = requester.map_or("◌", |requester| {
        loading_glyph(
            start,
            MotionMode::from_animations_enabled(animations),
            requester,
        )
    });
    clipped_line(Line::from(format!("  └ {glyph}")).dim(), width)
}

pub(super) fn error(text: &str, width: u16) -> HyperlinkLine {
    // Error status is concise and deliberately excludes potentially sensitive result bodies.
    clipped_line(Line::from(format!("  └ {text}")).red(), width)
}
