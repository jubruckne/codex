//! Records automatic daemon startup and ensures launch observations survive setup failures.

use super::*;
use crate::terminal_size_telemetry::LaunchTags;

/// Emit once after connection, or as unconfirmed if startup returns early.
pub(super) struct Launch<F: FnOnce(&AppServerTarget, bool) -> LaunchTags>(pub(super) Option<F>);

impl<F: FnOnce(&AppServerTarget, bool) -> LaunchTags> Launch<F> {
    pub(super) fn record(mut self, target: &AppServerTarget, connected: bool) -> LaunchTags {
        self.0
            .take()
            .map(|record| record(target, connected))
            .unwrap_or_default()
    }
}

impl<F: FnOnce(&AppServerTarget, bool) -> LaunchTags> Drop for Launch<F> {
    fn drop(&mut self) {
        if let Some(record) = self.0.take() {
            record(&AppServerTarget::Embedded, /*connected*/ false);
        }
    }
}
