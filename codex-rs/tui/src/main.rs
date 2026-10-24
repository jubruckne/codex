// Release builds exceed rustc's default query depth when laying out the async
// MessageProcessor::handle_initialized_client_request future.
#![recursion_limit = "256"]

use clap::Args;
use clap::Parser;
use codex_arg0::Arg0DispatchPaths;
use codex_config::LoaderOverrides;
use codex_tui::Cli;
use codex_tui::ExitReason;
use codex_tui::run_main;
use codex_utils_cli::CliConfigOverrides;
use std::io::Write;
use supports_color::Stream;

#[derive(Parser, Debug)]
#[command(
    name = codex_tui::CODEX_PLUS_NAME,
    bin_name = "codex++",
    version = codex_tui::CODEX_PLUS_VERSION_SUFFIX,
    about = codex_tui::CODEX_PLUS_IDENTITY,
    subcommand_negates_reqs = true
)]
struct TopCli {
    #[clap(flatten)]
    config_overrides: CliConfigOverrides,

    #[clap(flatten)]
    inner: Cli,

    #[clap(subcommand)]
    subcommand: Option<Subcommand>,
}

#[derive(clap::Subcommand, Debug)]
enum Subcommand {
    /// Resume a previous interactive session (picker by default; use --last to continue the most recent).
    Resume(ResumeCommand),
}

#[derive(Parser, Debug)]
#[command(
    display_name = codex_tui::CODEX_PLUS_NAME,
    version = codex_tui::CODEX_PLUS_VERSION_SUFFIX
)]
struct ResumeCommand {
    /// Session id (UUID) or session name. UUIDs take precedence if it parses.
    /// If omitted, use --last to pick the most recent recorded session.
    #[arg(value_name = "SESSION_ID")]
    session_id: Option<String>,

    /// Continue the most recent session without showing the picker.
    #[arg(long)]
    last: bool,

    /// Show all sessions (disables cwd filtering and shows CWD column).
    #[arg(long)]
    all: bool,

    /// Include non-interactive sessions in the resume picker and --last selection.
    #[arg(long)]
    include_non_interactive: bool,

    #[clap(flatten)]
    inner: SessionTuiCli,
}

// Match the upstream wrapper: --last PROMPT is valid, but --last SESSION_ID PROMPT is not.
#[derive(Debug)]
struct SessionTuiCli(Cli);

impl Args for SessionTuiCli {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        Cli::augment_args(cmd).mut_arg("prompt", |arg| arg.conflicts_with("last"))
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        Cli::augment_args_for_update(cmd).mut_arg("prompt", |arg| arg.conflicts_with("last"))
    }
}

impl clap::FromArgMatches for SessionTuiCli {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        Cli::from_arg_matches(matches).map(Self)
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches(matches)
    }
}

impl TopCli {
    fn into_tui_cli(self) -> Cli {
        let Self {
            mut config_overrides,
            mut inner,
            subcommand,
        } = self;
        inner
            .shared
            .take_auto_review_config_overrides(&mut config_overrides);
        if let Some(Subcommand::Resume(resume)) = subcommand {
            let mut resume_cli = resume.inner.0;
            let session_id = if resume.last && resume_cli.prompt.is_none() {
                resume_cli.prompt = resume.session_id;
                None
            } else {
                resume.session_id
            };
            inner.resume_picker = session_id.is_none() && !resume.last;
            inner.resume_last = resume.last;
            inner.resume_session_id = session_id;
            inner.resume_show_all = resume.all;
            inner.resume_include_non_interactive = resume.include_non_interactive;
            merge_interactive_cli_flags(&mut inner, resume_cli);
        }
        inner
            .config_overrides
            .prepend_root_overrides(config_overrides);
        disable_daemon_auto_start(&mut inner);
        inner
    }
}

// Match upstream arg0 runtime sizing without initializing backend helper aliases.
const TUI_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> anyhow::Result<()> {
    codex_build_info::initialize!();
    let handle = std::thread::Builder::new()
        .name("codex-plus-plus-main".into())
        .stack_size(TUI_STACK_SIZE)
        .spawn(|| -> anyhow::Result<()> {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(TUI_STACK_SIZE)
                .build()?;
            runtime.block_on(async {
                let arg0_paths = Arg0DispatchPaths {
                    codex_self_exe: std::env::current_exe().ok(),
                    ..Default::default()
                };
                let inner = TopCli::parse().into_tui_cli();
                let exit_info = run_main(
                    inner,
                    arg0_paths,
                    LoaderOverrides::default(),
                    /*explicit_remote_endpoint*/ None,
                )
                .await?;
                let is_fatal = match &exit_info.exit_reason {
                    ExitReason::Fatal(message) => {
                        eprintln!("ERROR: {message}");
                        true
                    }
                    ExitReason::UserRequested
                    | ExitReason::Archived(_)
                    | ExitReason::TurnInterrupted
                    | ExitReason::ThreadRemoved => false,
                };

                let color_enabled = supports_color::on(Stream::Stdout).is_some();
                for line in exit_info
                    .format_exit_messages_with_resume_executable(color_enabled, "codex++")
                {
                    println!("{line}");
                }
                if is_fatal {
                    std::io::stdout().flush()?;
                    std::process::exit(1);
                }
                Ok(())
            })
        })?;
    match handle.join() {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

// Keep resume-scoped option precedence aligned with the upstream CLI.
fn merge_interactive_cli_flags(interactive: &mut Cli, subcommand_cli: Cli) {
    let Cli {
        shared,
        strict_config,
        approval_policy,
        web_search,
        no_alt_screen,
        no_daemon,
        prompt,
        mut config_overrides,
        ..
    } = subcommand_cli;
    let subcommand_auto_review = shared.auto_review;
    interactive
        .shared
        .apply_subcommand_overrides(shared.into_inner());
    interactive
        .shared
        .take_auto_review_config_overrides(&mut config_overrides);
    if subcommand_auto_review {
        interactive.approval_policy = None;
    } else if let Some(approval) = approval_policy {
        interactive.approval_policy = Some(approval);
    }
    if web_search {
        interactive.web_search = true;
    }
    interactive.no_alt_screen |= no_alt_screen;
    interactive.no_daemon |= no_daemon;
    if strict_config {
        interactive.strict_config = true;
    }
    if let Some(prompt) = prompt {
        // Normalize CRLF/CR to LF so CLI-provided text can't leak `\r` into TUI state.
        interactive.prompt = Some(prompt.replace("\r\n", "\n").replace('\r', "\n"));
    }

    interactive
        .config_overrides
        .raw_overrides
        .extend(config_overrides.raw_overrides);
}

// This standalone client only attaches; daemon lifecycle belongs to the official CLI.
// Append last so neither user config nor a command-line override can enable auto-start.
fn disable_daemon_auto_start(cli: &mut Cli) {
    cli.config_overrides
        .raw_overrides
        .push("features.daemon_auto_start=false".to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_tui(args: &[&str]) -> Cli {
        TopCli::try_parse_from(args).unwrap().into_tui_cli()
    }

    #[test]
    fn resume_opens_picker_instead_of_submitting_a_prompt() {
        let cli = parse_tui(&["codex++", "resume"]);
        assert!(cli.resume_picker);
        assert!(!cli.resume_last);
        assert!(cli.resume_session_id.is_none());
        assert!(cli.prompt.is_none());
    }

    #[test]
    fn resume_last_accepts_an_optional_prompt() {
        for prompt in [None, Some("/compact focus on auth")] {
            let mut args = vec!["codex++", "resume", "--last"];
            args.extend(prompt);
            let cli = parse_tui(&args);
            assert!(cli.resume_last);
            assert!(!cli.resume_picker);
            assert!(cli.resume_session_id.is_none());
            assert_eq!(cli.prompt.as_deref(), prompt);
        }
    }

    #[test]
    fn resume_last_rejects_explicit_session_and_prompt() {
        let err = TopCli::try_parse_from([
            "codex++",
            "resume",
            "--last",
            "session-id",
            "continue here",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn resume_accepts_session_id_or_name_and_optional_prompt() {
        for session in ["123e4567-e89b-12d3-a456-426614174000", "my session"] {
            for prompt in [None, Some("continue here")] {
                let mut args = vec!["codex++", "resume", session];
                args.extend(prompt);
                let cli = parse_tui(&args);
                assert!(!cli.resume_picker);
                assert!(!cli.resume_last);
                assert_eq!(cli.resume_session_id.as_deref(), Some(session));
                assert_eq!(cli.prompt.as_deref(), prompt);
            }
        }
    }

    #[test]
    fn resume_preserves_picker_filters() {
        for selection in [None, Some("--last")] {
            let mut args = vec!["codex++", "resume", "--all", "--include-non-interactive"];
            args.extend(selection);
            let cli = parse_tui(&args);
            assert!(cli.resume_show_all);
            assert!(cli.resume_include_non_interactive);
            assert_eq!(cli.resume_picker, selection.is_none());
            assert_eq!(cli.resume_last, selection.is_some());
        }
    }

    #[test]
    fn resume_accepts_options_on_both_sides_with_subcommand_precedence() {
        let cli = parse_tui(&[
            "codex++",
            "-m",
            "root-model",
            "-C",
            "/root-dir",
            "--no-alt-screen",
            "resume",
            "--last",
            "-m",
            "resume-model",
            "-C",
            "/session-dir",
            "--search",
            "--strict-config",
            "-i",
            "/tmp/a.png,/tmp/b.png",
        ]);
        assert_eq!(cli.model.as_deref(), Some("resume-model"));
        assert_eq!(
            cli.cwd.as_deref(),
            Some(std::path::Path::new("/session-dir"))
        );
        assert!(cli.no_alt_screen);
        assert!(cli.web_search);
        assert!(cli.strict_config);
        assert_eq!(cli.images.len(), 2);
        assert_eq!(
            parse_tui(&["codex++", "-m", "root-model", "resume"]).model.as_deref(),
            Some("root-model")
        );
        assert!(parse_tui(&["codex++", "resume", "--no-alt-screen"]).no_alt_screen);
    }

    #[test]
    fn resume_preserves_config_order_and_cannot_enable_daemon_start() {
        let cli = parse_tui(&[
            "codex++",
            "-c",
            "model=before",
            "resume",
            "-c",
            "model=after",
            "-c",
            "features.daemon_auto_start=true",
        ]);
        let overrides = cli.config_overrides.parse_overrides().unwrap();
        let layer = codex_config::build_cli_overrides_layer(&overrides);
        assert_eq!(layer["model"].as_str(), Some("after"));
        assert_eq!(
            layer["features"]["daemon_auto_start"].as_bool(),
            Some(false)
        );
        assert!(cli.daemon_cli_executable.is_none());
        for args in [
            ["codex++", "--no-daemon", "resume"],
            ["codex++", "resume", "--no-daemon"],
        ] {
            assert!(parse_tui(&args).no_daemon);
        }
    }

    #[test]
    fn resume_approval_modes_have_upstream_precedence() {
        let cli = parse_tui(&["codex++", "-a", "never", "resume", "--approve-for-me"]);
        assert!(cli.approval_policy.is_none());
        let overrides = cli.config_overrides.parse_overrides().unwrap();
        let layer = codex_config::build_cli_overrides_layer(&overrides);
        assert_eq!(layer["approval_policy"].as_str(), Some("on-request"));
        assert_eq!(layer["approvals_reviewer"].as_str(), Some("auto_review"));

        let cli = parse_tui(&["codex++", "--approve-for-me", "resume", "-a", "never"]);
        assert!(matches!(
            cli.approval_policy,
            Some(codex_utils_cli::ApprovalModeCliArg::Never)
        ));
    }

    #[test]
    fn ordinary_prompts_and_escaped_resume_still_start_new_sessions() {
        for prompt in ["hello", "resume this work"] {
            let cli = parse_tui(&["codex++", prompt]);
            assert_eq!(cli.prompt.as_deref(), Some(prompt));
            assert!(!cli.resume_picker);
            assert!(!cli.resume_last);
        }
        let cli = parse_tui(&["codex++", "--", "resume"]);
        assert_eq!(cli.prompt.as_deref(), Some("resume"));
        assert!(!cli.resume_picker);
    }

    #[test]
    fn resume_help_explains_picker_and_selection_options() {
        let err = TopCli::try_parse_from(["codex++", "resume", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = err.to_string();
        assert!(help.contains("Usage: codex++ resume"));
        for option in [
            "[SESSION_ID]",
            "[PROMPT]",
            "--last",
            "--all",
            "--include-non-interactive",
        ] {
            assert!(help.contains(option), "missing {option}: {help}");
        }
    }

    #[test]
    fn resume_exit_hints_use_codex_plus_and_preserve_official_stop_command() {
        let id =
            codex_protocol::ThreadId::from_string("123e4567-e89b-12d3-a456-426614174000").unwrap();
        let mut info = codex_tui::AppExitInfo {
            token_usage: Default::default(),
            thread_id: Some(id),
            resume_hint: Some(codex_tui::ResumableThread {
                thread_id: id,
                thread_name: Some("my session".into()),
            }),
            disconnect_info: None,
            update_action: None,
            exit_reason: ExitReason::UserRequested,
        };
        let lines = info
            .clone()
            .format_exit_messages_with_resume_executable(false, "codex++");
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("  codex++ resume "))
        );
        assert!(lines.contains(&"Or run codex++ resume and select my session.".to_string()));
        assert!(
            info.clone()
                .format_exit_messages(false)
                .iter()
                .any(|line| line.starts_with("  codex resume "))
        );
        info.disconnect_info = Some(codex_tui::DisconnectInfo {
            command: vec!["codex".into()],
            stop_hint: "press x".into(),
        });
        let lines = info.format_exit_messages_with_resume_executable(false, "codex++");
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("  codex++ resume "))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("run codex agents, select this task"))
        );
    }

    #[test]
    fn codex_plus_cli_identity_is_consistent_for_help_and_version() {
        for flag in ["--version", "-V"] {
            for command in [None, Some("resume")] {
                let mut args = vec!["codex++"];
                args.extend(command);
                args.push(flag);
                let error = TopCli::try_parse_from(args).unwrap_err();
                assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
                assert_eq!(
                    error.to_string(),
                    format!("{}\n", codex_tui::CODEX_PLUS_IDENTITY)
                );
            }
        }
        let error = TopCli::try_parse_from(["codex++", "--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = error.to_string();
        assert!(help.starts_with(codex_tui::CODEX_PLUS_IDENTITY));
        assert!(help.contains("Usage: codex++"));
        assert!(!help.contains("codex-custom"));
    }

    #[test]
    fn fullscreen_launch_retains_local_auto_start_guard() {
        let cli = parse_tui(&["codex++", "-c", "tui.fullscreen_transcript=true"]);
        let overrides = cli.config_overrides.parse_overrides().unwrap();
        let layer = codex_config::build_cli_overrides_layer(&overrides);
        assert_eq!(layer["tui"]["fullscreen_transcript"].as_bool(), Some(true));
        assert_eq!(layer["features"]["daemon_auto_start"].as_bool(), Some(false));
    }

    #[test]
    fn custom_entry_point_disables_daemon_start_after_user_overrides() {
        let mut cli = Cli::parse_from(["codex++"]);
        cli.config_overrides
            .raw_overrides
            .push("features.daemon_auto_start=true".into());
        disable_daemon_auto_start(&mut cli);
        let overrides = cli.config_overrides.parse_overrides().unwrap();
        let layer = codex_config::build_cli_overrides_layer(&overrides);
        assert_eq!(
            layer["features"]["daemon_auto_start"].as_bool(),
            Some(false)
        );
        assert!(cli.daemon_cli_executable.is_none());
    }
}
