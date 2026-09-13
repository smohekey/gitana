//! `gta-mcp` — normal one-shot command execution plus path-aware MCP stdio/HTTP serving.

mod cli;
mod git_path;
mod mcp_bridge;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::parser::ValueSource;
use clap::{Arg, ArgAction, CommandFactory, FromArgMatches};

const EXPORT_SKILLS_ID: &str = "export-skills";

#[derive(Debug)]
enum ServeMode {
	Stdio,
	Http(SocketAddr),
}

enum EntryMode {
	Command(cli::Cli),
	Serve(ServeMode),
	ExportSkills(Option<PathBuf>),
}

fn main() -> ExitCode {
	let mode = match entry_mode() {
		Ok(mode) => mode,
		Err(error) => error.exit(),
	};
	let result = match mode {
		EntryMode::Command(cli) => cli::execute(cli),
		EntryMode::Serve(mode) => tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.map_err(anyhow::Error::from)
			.and_then(|runtime| {
				runtime.block_on(async {
					match mode {
						ServeMode::Stdio => mcp_bridge::serve_stdio().await,
						ServeMode::Http(address) => mcp_bridge::serve_http(address).await,
					}
				})
			}),
		EntryMode::ExportSkills(directory) => mcp_bridge::export_skills(directory),
	};
	match result {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			if let Some(silent) = error.downcast_ref::<gta_core::SilentExit>() {
				eprintln!("{}", silent.reason);
			} else {
				eprintln!(
					"gta-mcp: {}",
					gta_core::render_error(&error, gta_core::ResultPathMode::Reversible)
				);
			}
			ExitCode::FAILURE
		}
	}
}

fn entry_mode() -> Result<EntryMode, clap::Error> {
	entry_mode_from(std::env::args().skip(1), |name| std::env::var(name).ok())
}

fn entry_mode_from(
	arguments: impl IntoIterator<Item = String>,
	environment: impl Fn(&str) -> Option<String>,
) -> Result<EntryMode, clap::Error> {
	let mut argv = vec!["gta-mcp".to_owned()];
	argv.extend(arguments);
	let mut command = entry_command();
	let matches = command.clone().try_get_matches_from(argv)?;
	let export_requested = matches.value_source(EXPORT_SKILLS_ID) == Some(ValueSource::CommandLine);
	let http_requested = matches.value_source("mcp-http") == Some(ValueSource::CommandLine);
	let stdio_requested = matches.get_flag("mcp");
	if (export_requested || http_requested || stdio_requested) && matches.subcommand_name().is_some()
	{
		return Err(command.error(
			ErrorKind::ArgumentConflict,
			"MCP serving and skills export modes cannot be combined with a command",
		));
	}
	if export_requested {
		let directory = matches
			.get_one::<String>(EXPORT_SKILLS_ID)
			.filter(|directory| !directory.is_empty())
			.map(PathBuf::from);
		return Ok(EntryMode::ExportSkills(directory));
	}
	if http_requested {
		let explicit = matches
			.get_one::<String>("mcp-http")
			.filter(|address| !address.is_empty())
			.map(String::as_str);
		let address = resolve_http_listen(explicit, &environment)
			.map_err(|error| command.error(ErrorKind::InvalidValue, error.to_string()))?;
		return Ok(EntryMode::Serve(ServeMode::Http(address)));
	}
	if stdio_requested {
		return Ok(EntryMode::Serve(ServeMode::Stdio));
	}
	cli::Cli::from_arg_matches(&matches).map(EntryMode::Command)
}

fn entry_command() -> clap::Command {
	cli::Cli::command()
		.subcommand_required(false)
		.arg(
			Arg::new(EXPORT_SKILLS_ID)
				.long(EXPORT_SKILLS_ID)
				.value_name("DIR")
				.help("Generate Agent Skills (SKILL.md) and exit")
				.action(ArgAction::Set)
				.num_args(0..=1)
				.default_missing_value("")
				.conflicts_with_all(["mcp", "mcp-http"]),
		)
		.mut_arg("mcp", |argument| {
			argument.conflicts_with_all(["mcp-http", EXPORT_SKILLS_ID])
		})
		.mut_arg("mcp-http", |argument| {
			argument.conflicts_with_all(["mcp", EXPORT_SKILLS_ID])
		})
}

fn resolve_http_listen(
	explicit: Option<&str>,
	environment: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<SocketAddr> {
	let configured = explicit.map(ToOwned::to_owned).or_else(|| {
		environment("CLAP_MCP_HTTP_LISTEN")
			.filter(|listen| !listen.is_empty())
			.or_else(|| {
				let bind = environment("CLAP_MCP_HTTP_BIND").filter(|bind| !bind.is_empty())?;
				let port = environment("CLAP_MCP_HTTP_PORT").filter(|port| !port.is_empty())?;
				Some(format!("{bind}:{port}"))
			})
	});
	let configured = configured.ok_or_else(|| {
		anyhow::anyhow!(
			"--mcp-http requires HOST:PORT, or set CLAP_MCP_HTTP_LISTEN, or \
			 CLAP_MCP_HTTP_BIND + CLAP_MCP_HTTP_PORT"
		)
	})?;
	let address = configured.parse().map_err(|_| {
		anyhow::anyhow!("invalid MCP HTTP listen address `{configured}` (expected host:port)")
	})?;
	mcp_bridge::validate_http_address(address)?;
	Ok(address)
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use clap::error::ErrorKind;

	use super::{EntryMode, ServeMode, entry_mode_from};

	fn no_environment(_name: &str) -> Option<String> {
		None
	}

	fn expect_error(result: Result<EntryMode, clap::Error>) -> clap::Error {
		match result {
			Ok(_) => panic!("expected argument parsing to fail"),
			Err(error) => error,
		}
	}

	#[test]
	fn serving_flags_are_only_recognized_before_the_command_path() {
		assert!(matches!(
			entry_mode_from(["--mcp".to_owned()], no_environment).unwrap(),
			EntryMode::Serve(ServeMode::Stdio)
		));
		assert!(matches!(
			entry_mode_from(
				["add".to_owned(), "--".to_owned(), "--mcp".to_owned()],
				no_environment,
			)
			.unwrap(),
			EntryMode::Command(_)
		));
		assert!(matches!(
			entry_mode_from(
				["-C".to_owned(), "--mcp".to_owned(), "status".to_owned()],
				no_environment,
			)
			.unwrap(),
			EntryMode::Command(_)
		));
	}

	#[test]
	fn complete_invocation_is_validated_before_selecting_a_server() {
		for arguments in [
			vec!["--mcp", "--bogus"],
			vec!["--bogus", "--mcp"],
			vec!["--mcp-http", "127.0.0.1:7000", "--bogus"],
		] {
			let error = expect_error(entry_mode_from(
				arguments.into_iter().map(str::to_owned),
				no_environment,
			));
			assert_eq!(error.kind(), ErrorKind::UnknownArgument);
		}

		let conflict = expect_error(entry_mode_from(
			["--mcp".to_owned(), "status".to_owned()],
			no_environment,
		));
		assert_eq!(conflict.kind(), ErrorKind::ArgumentConflict);

		let version = expect_error(entry_mode_from(
			["--version".to_owned(), "--mcp".to_owned()],
			no_environment,
		));
		assert_eq!(version.kind(), ErrorKind::DisplayVersion);
	}

	#[test]
	fn http_listen_uses_explicit_then_environment_configuration() {
		let explicit = entry_mode_from(["--mcp-http=127.0.0.1:7000".to_owned()], |_| {
			Some("127.0.0.1:8000".to_owned())
		})
		.unwrap();
		assert!(matches!(
			explicit,
			EntryMode::Serve(ServeMode::Http(address))
				if address == "127.0.0.1:7000".parse().unwrap()
		));

		let listen = entry_mode_from(["--mcp-http".to_owned()], |name| {
			(name == "CLAP_MCP_HTTP_LISTEN").then(|| "127.0.0.1:7001".to_owned())
		})
		.unwrap();
		assert!(matches!(
			listen,
			EntryMode::Serve(ServeMode::Http(address))
				if address == "127.0.0.1:7001".parse().unwrap()
		));

		let split = entry_mode_from(["--mcp-http=".to_owned()], |name| match name {
			"CLAP_MCP_HTTP_BIND" => Some("127.0.0.1".to_owned()),
			"CLAP_MCP_HTTP_PORT" => Some("7002".to_owned()),
			_ => None,
		})
		.unwrap();
		assert!(matches!(
			split,
			EntryMode::Serve(ServeMode::Http(address))
				if address == "127.0.0.1:7002".parse().unwrap()
		));
	}

	#[test]
	fn http_listen_rejects_missing_or_malformed_configuration() {
		let missing = expect_error(entry_mode_from(["--mcp-http".to_owned()], no_environment));
		assert!(missing.to_string().contains("CLAP_MCP_HTTP_LISTEN"));

		let malformed = expect_error(entry_mode_from(["--mcp-http".to_owned()], |name| {
			(name == "CLAP_MCP_HTTP_LISTEN").then(|| "not-an-address".to_owned())
		}));
		assert!(
			malformed
				.to_string()
				.contains("invalid MCP HTTP listen address")
		);

		for configured in [
			"0.0.0.0:7000",
			"192.0.2.10:7000",
			"[::]:7000",
			"[2001:db8::10]:7000",
		] {
			let rejected = expect_error(entry_mode_from(
				[format!("--mcp-http={configured}")],
				no_environment,
			));
			assert!(rejected.to_string().contains("loopback"), "{configured}");
		}

		let environment = expect_error(entry_mode_from(["--mcp-http".to_owned()], |name| {
			(name == "CLAP_MCP_HTTP_LISTEN").then(|| "192.0.2.10:7000".to_owned())
		}));
		assert!(environment.to_string().contains("loopback"));
	}

	#[test]
	fn skills_export_accepts_default_separate_and_attached_directories() {
		assert!(matches!(
			entry_mode_from(["--export-skills".to_owned()], no_environment).unwrap(),
			EntryMode::ExportSkills(None)
		));
		assert!(matches!(
			entry_mode_from(
				["--export-skills".to_owned(), "skills".to_owned()],
				no_environment,
			)
			.unwrap(),
			EntryMode::ExportSkills(Some(directory)) if directory == Path::new("skills")
		));
		assert!(matches!(
			entry_mode_from(["--export-skills=other".to_owned()], no_environment).unwrap(),
			EntryMode::ExportSkills(Some(directory)) if directory == Path::new("other")
		));
	}
}
