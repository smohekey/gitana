use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;

/// The git-related environment that config, ignore-file and attributes-file lookups read, given in
/// place of the process's own by [`with_environment`]: for a caller that must keep the person's own
/// setup out (a test), or that supplies one of its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Environment {
	/// `HOME`: where `~` leads, and where the global files are by default. `None`: no home.
	pub home: Option<PathBuf>,
	/// `XDG_CONFIG_HOME`: where the XDG config, ignore and attributes files are by default.
	pub xdg_config_home: Option<PathBuf>,
	/// `GIT_CONFIG_GLOBAL`: the global config file, in place of `~/.gitconfig` and the XDG one.
	pub config_global: Option<PathBuf>,
	/// `GIT_CONFIG_NOSYSTEM`: leave the system config file out.
	pub config_nosystem: bool,
	/// `GIT_ATTR_NOSYSTEM`: leave the system attributes file out.
	pub attr_nosystem: bool,
}

tokio::task_local! {
	/// The environment [`with_environment`] gives, for the task it runs.
	static ENVIRONMENT: Environment;
}

/// Run `future` with `environment` read, by every config, ignore-file and attributes-file lookup in
/// it, in place of the process's own: the variables it names as it gives them, and none of git's
/// others (no `GIT_CONFIG_SYSTEM`, no `GIT_CONFIG_COUNT` entries, and on Windows no home from
/// `USERPROFILE`). `PWD`, the shell's, is still the process's.
pub async fn with_environment<F: Future>(environment: Environment, future: F) -> F::Output {
	ENVIRONMENT.scope(environment, future).await
}

/// The environment the [`with_environment`] in force gives, if one is. A task doesn't inherit it
/// from the task that spawned it: work handed to another task takes it from here, and runs there
/// under [`with_environment`] too.
pub fn current_environment() -> Option<Environment> {
	ENVIRONMENT.try_with(Clone::clone).ok()
}

/// The environment variable `name`, as a lookup reads it: from the [`with_environment`] in force,
/// else from the process.
pub(crate) fn var_os(name: &str) -> Option<OsString> {
	ENVIRONMENT
		.try_with(|environment| {
			let flag = |set: bool| set.then(|| OsString::from("1"));
			match name {
				"HOME" => environment.home.clone().map(OsString::from),
				"XDG_CONFIG_HOME" => environment.xdg_config_home.clone().map(OsString::from),
				"GIT_CONFIG_GLOBAL" => environment.config_global.clone().map(OsString::from),
				"GIT_CONFIG_NOSYSTEM" => flag(environment.config_nosystem),
				"GIT_ATTR_NOSYSTEM" => flag(environment.attr_nosystem),
				_ => None,
			}
		})
		.unwrap_or_else(|_| std::env::var_os(name))
}

/// [`var_os`] as text, as `std::env::var` gives it.
pub(crate) fn var(name: &str) -> Result<String, std::env::VarError> {
	match var_os(name) {
		None => Err(std::env::VarError::NotPresent),
		Some(value) => value.into_string().map_err(std::env::VarError::NotUnicode),
	}
}

#[cfg(test)]
mod tests {
	use super::{Environment, current_environment, var_os, with_environment};

	/// Inside an override, the variables it names come from it and git's others are absent; outside,
	/// the process's.
	#[tokio::test]
	async fn an_override_stands_in_for_the_process_environment() {
		let environment = Environment {
			config_global: Some("/tmp/global".into()),
			config_nosystem: true,
			..Environment::default()
		};
		with_environment(environment, async {
			assert_eq!(
				var_os("GIT_CONFIG_GLOBAL").as_deref(),
				Some("/tmp/global".as_ref())
			);
			assert_eq!(var_os("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1".as_ref()));
			assert_eq!(var_os("GIT_ATTR_NOSYSTEM"), None);
			assert_eq!(var_os("HOME"), None);
			assert_eq!(var_os("GIT_CONFIG_COUNT"), None);
		})
		.await;
		assert_eq!(var_os("PATH"), std::env::var_os("PATH"));
	}

	/// The override in force is there to carry to another task; outside one, there's none.
	#[tokio::test]
	async fn the_override_in_force_can_be_carried_to_another_task() {
		let environment = Environment {
			home: Some("/tmp/home".into()),
			..Environment::default()
		};
		assert_eq!(current_environment(), None);
		let carried = with_environment(environment.clone(), async {
			let carried = current_environment();
			tokio::spawn(async move {
				// A spawned task doesn't inherit it, and gets it back as carried.
				assert_eq!(var_os("HOME"), std::env::var_os("HOME"));
				with_environment(carried.unwrap(), async { var_os("HOME") }).await
			})
			.await
			.unwrap()
		})
		.await;
		assert_eq!(carried.as_deref(), Some("/tmp/home".as_ref()));
	}
}
