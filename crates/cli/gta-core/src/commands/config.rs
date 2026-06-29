use std::path::Path;

use anyhow::{Result, anyhow, bail};

use crate::repo;

/// Read or write local repository configuration (`.git/config`).
///
/// With a `name` and `value`, sets the variable; with `name` only, prints its value (exit
/// non-zero if unset). `get_all` prints every value, `add` appends one, `unset` removes the
/// variable, and `list` prints all variables as `key=value`. `as_bool`/`as_int` interpret the
/// read value. Note: writing re-renders the whole file, so comments and exact layout are not
/// preserved.
#[allow(clippy::too_many_arguments)]
pub async fn run(
	cwd: &Path,
	get: bool,
	get_all: bool,
	add: bool,
	unset: bool,
	list: bool,
	as_bool: bool,
	as_int: bool,
	name: Option<String>,
	value: Option<String>,
) -> Result<()> {
	let repo = repo::open_here(cwd)?;

	if list {
		for (key, val) in repo.read_config().await?.entries() {
			match val {
				Some(v) => println!("{key}={v}"),
				None => println!("{key}"),
			}
		}
		return Ok(());
	}

	let name = name.ok_or_else(|| anyhow!("a config key is required"))?;
	let (section, subsection, var) = parse_key(&name)?;

	if unset {
		// Git treats a value with --unset as a value-pattern to match; we do not implement that
		// yet, so reject it rather than deleting the key regardless of its value.
		if value.is_some() {
			bail!("--unset does not take a value (value-pattern matching is not supported)");
		}
		let mut config = repo.read_config().await?;
		if !config.unset(section, subsection, var) {
			bail!("key '{name}' is not set");
		}
		return repo.write_config(&config).await.map_err(Into::into);
	}

	if add {
		let value = value.ok_or_else(|| anyhow!("--add requires a value"))?;
		let mut config = repo.read_config().await?;
		config.add(section, subsection, var, Some(&value));
		return repo.write_config(&config).await.map_err(Into::into);
	}

	// A value (without a read flag) means set.
	if let Some(value) = value {
		if get || get_all {
			bail!("a get option cannot take a value");
		}
		let mut config = repo.read_config().await?;
		config.set(section, subsection, var, &value);
		return repo.write_config(&config).await.map_err(Into::into);
	}

	// Otherwise read. A present-but-valueless variable (a bare boolean) prints an empty line and
	// succeeds, distinct from an absent key, which exits non-zero.
	let config = repo.read_config().await?;
	if get_all {
		let values = config.get_all_raw(section, subsection, var);
		if values.is_empty() {
			bail!("key '{name}' is not set");
		}
		for v in values {
			println!("{}", v.unwrap_or(""));
		}
		return Ok(());
	}

	let found = if as_bool {
		config
			.get_bool(section, subsection, var)?
			.map(|b| b.to_string())
	} else if as_int {
		config
			.get_int(section, subsection, var)?
			.map(|n| n.to_string())
	} else {
		config
			.get_raw(section, subsection, var)
			.map(|value| value.unwrap_or("").to_owned())
	};
	match found {
		Some(rendered) => println!("{rendered}"),
		None => bail!("key '{name}' is not set"),
	}
	Ok(())
}

/// Split a dotted config key into `(section, subsection, name)`: the first `.` ends the section,
/// the last `.` begins the name, and anything between is the (case-sensitive) subsection.
fn parse_key(key: &str) -> Result<(&str, Option<&str>, &str)> {
	let first = key
		.find('.')
		.ok_or_else(|| anyhow!("key '{key}' does not contain a section"))?;
	let last = key.rfind('.').unwrap();
	let section = &key[..first];
	let name = &key[last + 1..];
	if section.is_empty() || name.is_empty() {
		bail!("invalid config key: '{key}'");
	}
	let subsection = (first != last).then(|| &key[first + 1..last]);
	Ok((section, subsection, name))
}
