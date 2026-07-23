//! The `ssh` program family — which decides the port flag (and `-batch`) gitana passes.

/// The `ssh` program family. git passes the port (and, for TortoisePlink, `-batch`) differently per
/// family, so the transport must know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshVariant {
	/// OpenSSH (`ssh`) — the port is `-p <port>`.
	OpenSsh,
	/// The PuTTY family (`plink` / `putty`) — the port is `-P <port>` (uppercase), no `-o` options.
	Putty,
	/// TortoisePlink — like [`Putty`](Self::Putty) (`-P <port>`), plus `-batch` so an unattended
	/// operation never blocks on an interactive dialog (git adds this for `tortoiseplink`).
	TortoisePlink,
	/// A minimal wrapper (git's `simple` variant) that cannot set a port — a port request is an error.
	Simple,
}

impl SshVariant {
	/// Classify an ssh **program** by basename — git's default when no variant is set explicitly. The
	/// caller passes the resolved program (a program path, or a shell command's first word); this takes
	/// its basename (after any `/` or `\`) minus a `.exe` suffix. git only auto-detects the PuTTY plinks
	/// this way: `plink` → [`Putty`](Self::Putty), `tortoiseplink` → [`TortoisePlink`](Self::TortoisePlink).
	/// Everything else — including `putty` and `simple`, which git recognises only as *explicit* variants,
	/// not by basename — is [`OpenSsh`](Self::OpenSsh) (git's runtime `-G` probe, which would otherwise
	/// confirm OpenSSH or fall back to `simple`, is deliberately omitted).
	pub fn detect(program: &str) -> Self {
		let base = program
			.rsplit(['/', '\\'])
			.next()
			.unwrap_or(program)
			.to_ascii_lowercase();
		let base = base.strip_suffix(".exe").unwrap_or(&base);
		match base {
			"plink" => SshVariant::Putty,
			"tortoiseplink" => SshVariant::TortoisePlink,
			_ => SshVariant::OpenSsh,
		}
	}

	/// Parse an explicit `ssh.variant` / `GIT_SSH_VARIANT` value, matching git's **case-sensitive**
	/// handling: `plink` / `putty` → PuTTY, `tortoiseplink` → TortoisePlink, `simple` → Simple. Only the
	/// exact value `auto` returns `None` (defer to [`detect`](Self::detect)); every other value —
	/// including `ssh`, a misspelling, a wrong case like `PLINK`, or an empty string — falls back to
	/// [`OpenSsh`](Self::OpenSsh) (git's `VARIANT_SSH` default), *not* basename auto-detection.
	pub fn parse(value: &str) -> Option<Self> {
		match value {
			"auto" => None,
			"plink" | "putty" => Some(SshVariant::Putty),
			"tortoiseplink" => Some(SshVariant::TortoisePlink),
			"simple" => Some(SshVariant::Simple),
			_ => Some(SshVariant::OpenSsh),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn detects_only_plink_family_by_basename() {
		assert_eq!(SshVariant::detect("ssh"), SshVariant::OpenSsh);
		assert_eq!(SshVariant::detect("/usr/bin/plink"), SshVariant::Putty);
		assert_eq!(
			SshVariant::detect("TortoisePlink.exe"),
			SshVariant::TortoisePlink
		);
		// A full Windows path is basenamed on `\`.
		assert_eq!(
			SshVariant::detect("C:\\Program Files\\PuTTY\\plink.exe"),
			SshVariant::Putty
		);
		// git does NOT auto-detect `putty` or `simple` by basename — only as explicit variants — so a
		// program so named is OpenSSH here (the `-G` probe is omitted).
		assert_eq!(SshVariant::detect("putty"), SshVariant::OpenSsh);
		assert_eq!(SshVariant::detect("simple"), SshVariant::OpenSsh);
		assert_eq!(SshVariant::detect("/opt/custom-ssh"), SshVariant::OpenSsh);
	}

	#[test]
	fn parses_explicit_variant_case_sensitively() {
		assert_eq!(SshVariant::parse("ssh"), Some(SshVariant::OpenSsh));
		assert_eq!(SshVariant::parse("plink"), Some(SshVariant::Putty));
		assert_eq!(
			SshVariant::parse("tortoiseplink"),
			Some(SshVariant::TortoisePlink)
		);
		assert_eq!(SshVariant::parse("simple"), Some(SshVariant::Simple));
		// Only exact `auto` defers to basename detection.
		assert_eq!(SshVariant::parse("auto"), None);
		// A wrong case, misspelling, or empty value falls back to OpenSSH (git is case-sensitive here).
		assert_eq!(SshVariant::parse("PLINK"), Some(SshVariant::OpenSsh));
		assert_eq!(SshVariant::parse("weird"), Some(SshVariant::OpenSsh));
		assert_eq!(SshVariant::parse(""), Some(SshVariant::OpenSsh));
	}
}
