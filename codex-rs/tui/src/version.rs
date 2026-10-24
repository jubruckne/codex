/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Fork identity for display. Transport and compatibility versions remain numeric.
pub const CODEX_PLUS_NAME: &str = "Codex++";
pub const CODEX_PLUS_VERSION_SUFFIX: &str =
    concat!("(based on OpenAI Codex ", env!("CARGO_PKG_VERSION"), ")");
pub const CODEX_PLUS_IDENTITY: &str =
    concat!("Codex++ (based on OpenAI Codex ", env!("CARGO_PKG_VERSION"), ")");

pub(crate) fn codex_plus_identity(version: &str) -> String {
    format!("{CODEX_PLUS_NAME} (based on OpenAI Codex {version})")
}
