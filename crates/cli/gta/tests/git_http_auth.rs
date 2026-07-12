//! HTTP Basic-auth credential flow for the `gta` client, end to end.
//!
//! A loopback axum server ([`support::serve_gitana_basic_auth`]) serves gitana's own Smart-HTTP
//! handlers behind `401 WWW-Authenticate: Basic`, and the real `gta` binary (subprocess) must acquire
//! and present credentials to get through — from the URL userinfo, from a saved `remote.origin.url`
//! username plus a scripted `GIT_ASKPASS`, and correctly *failing* on a wrong password (slice 1); and
//! from configured credential *helpers* (`git-credential-*` programs), which supply a credential
//! before any prompt and persist/erase it on `approve`/`reject` (slice 2).
//!
//! Every case runs with the ambient global/system gitconfig neutralised (`gta_iso`), so a developer's
//! real `credential.helper` (e.g. osxkeychain) never leaks in — the tests configure exactly the
//! helper they mean to exercise, and no other.

mod support;

use std::path::Path;

use gitana_object::Sha256;
use gitana_repository::{FileMode, TreeBuildEntry};
use support::{ServerHash, gta_env, open, serve_gitana_basic_auth, unique_tmp};

/// A fixed identity for the server-side seed commit (`Name <email> seconds ±hhmm`).
const WHO: &str = "A U Thor <a@example.com> 0 +0000";

/// `gta` with the ambient global/system gitconfig neutralised (`/dev/null`), so these hermetic auth
/// tests never consult a developer's real `credential.helper`. Extra `env` is appended, and — since
/// the process applies env last-wins — a test may re-point `GIT_CONFIG_GLOBAL` at its own config file
/// to install a helper deliberately.
async fn gta_iso(args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
	let mut full = vec![
		("GIT_CONFIG_GLOBAL", "/dev/null"),
		("GIT_CONFIG_SYSTEM", "/dev/null"),
	];
	full.extend_from_slice(env);
	gta_env(args, &full).await
}

/// Write an executable file-backed credential helper (unix only). It speaks git's `get`/`store`/`erase`
/// protocol over a flat `protocol://host \t user \t pass` store at `$HELPER_FILE`, keyed on
/// protocol+host — a miniature `git-credential-store`, but self-contained so the round-trip is
/// observable without depending on a real helper.
#[cfg(unix)]
fn write_store_helper(path: &Path) {
	use std::os::unix::fs::PermissionsExt;
	let script = r#"#!/bin/sh
op="$1"; f="$HELPER_FILE"; TAB=$(printf '\t')
proto= host= user= pass=
while IFS= read -r line; do
  [ -z "$line" ] && break
  k=${line%%=*}; v=${line#*=}
  case "$k" in
    protocol) proto=$v ;; host) host=$v ;; username) user=$v ;; password) pass=$v ;;
  esac
done
key="$proto://$host"
case "$op" in
  get)
    [ -f "$f" ] || exit 0
    while IFS="$TAB" read -r k u p; do
      [ "$k" = "$key" ] && { printf 'username=%s\npassword=%s\n' "$u" "$p"; break; }
    done < "$f" ;;
  store)
    tmp="$f.$$"
    [ -f "$f" ] && grep -v "^$key$TAB" "$f" > "$tmp" 2>/dev/null
    printf '%s%s%s%s%s\n' "$key" "$TAB" "$user" "$TAB" "$pass" >> "$tmp"
    mv "$tmp" "$f" ;;
  erase)
    [ -f "$f" ] || exit 0
    tmp="$f.$$"; grep -v "^$key$TAB" "$f" > "$tmp" 2>/dev/null || true; mv "$tmp" "$f" ;;
esac
exit 0
"#;
	std::fs::write(path, script).unwrap();
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Write a global gitconfig that installs `helpers` (absolute paths) as the `credential.helper` chain,
/// in order, returning the config path to hand to `GIT_CONFIG_GLOBAL`.
fn write_helper_config(dir: &Path, helpers: &[&Path]) -> std::path::PathBuf {
	let mut text = String::from("[credential]\n");
	for helper in helpers {
		text.push_str(&format!("\thelper = {}\n", helper.display()));
	}
	let config = dir.join("global.gitconfig");
	std::fs::write(&config, text).unwrap();
	config
}

/// Write an executable `#!/bin/sh` script with `body` (unix only).
#[cfg(unix)]
fn write_exec(path: &Path, body: &str) {
	use std::os::unix::fs::PermissionsExt;
	std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Initialise a SHA-256 server repo at `git_dir` with a single commit holding `hello.txt`.
async fn seed(git_dir: &Path) {
	std::fs::create_dir_all(git_dir).unwrap();
	let repo = open::<Sha256>(git_dir);
	repo.init().await.unwrap();
	commit_file(git_dir, "hello.txt", b"hello\n").await;
}

/// Add a commit on the server repo's HEAD introducing `file` with `content`.
async fn commit_file(git_dir: &Path, file: &str, content: &[u8]) {
	let repo = open::<Sha256>(git_dir);
	let blob = repo.write_blob(content).await.unwrap();
	let tree = repo
		.write_tree(&[TreeBuildEntry {
			path: file.to_owned(),
			mode: FileMode::Regular,
			id: blob,
		}])
		.await
		.unwrap();
	repo.commit_on_head(tree, WHO, WHO, "srv\n").await.unwrap();
}

/// Insert `userinfo` (`user` or `user:pass`) into an `http://host…` URL.
fn with_userinfo(url: &str, userinfo: &str) -> String {
	url.replacen("http://", &format!("http://{userinfo}@"), 1)
}

/// Write an executable `askpass` script that echoes `answer` (unix only; the test suite runs on
/// unix). git invokes it as `askpass "<prompt>"` and reads the answer from stdout.
#[cfg(unix)]
fn write_askpass(path: &Path, answer: &str) {
	use std::os::unix::fs::PermissionsExt;
	std::fs::write(path, format!("#!/bin/sh\necho '{answer}'\n")).unwrap();
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A credential embedded in the clone URL as `user:pass@` authenticates the clone, and the saved
/// `remote.origin.url` keeps the username but not the password (matching git).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_authenticates_from_url_userinfo() {
	let work = unique_tmp("auth-url");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir, ServerHash::Sha256, "alice", "s3cr3t").await;

	let checkout = work.join("c");
	let out = gta_iso(
		&[
			"clone",
			&with_userinfo(&url, "alice:s3cr3t"),
			checkout.to_str().unwrap(),
		],
		&[],
	)
	.await;
	assert!(
		out.status.success(),
		"authenticated clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(checkout.join("hello.txt").exists(), "checkout missing file");

	// The saved remote keeps the username as a hint but never the password.
	let config = std::fs::read_to_string(checkout.join(".git/config")).unwrap();
	assert!(
		config.contains("url = http://alice@127.0.0.1"),
		"expected a username-only saved url, got: {config}"
	);
	assert!(
		!config.contains("s3cr3t"),
		"the password leaked into config: {config}"
	);
	// The password must not leak into the clone reflog either (git anonymizes the URL there).
	let reflog = std::fs::read_to_string(checkout.join(".git/logs/HEAD")).unwrap();
	assert!(
		!reflog.contains("s3cr3t"),
		"the password leaked into the reflog: {reflog}"
	);
}

/// With only a username in the URL, the password is prompted for — a scripted `GIT_ASKPASS` supplies
/// it, and the clone succeeds.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_prompts_password_via_askpass() {
	let work = unique_tmp("auth-askpass");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir, ServerHash::Sha256, "alice", "s3cr3t").await;

	let askpass = work.join("askpass.sh");
	write_askpass(&askpass, "s3cr3t");

	let checkout = work.join("c");
	let out = gta_iso(
		&[
			"clone",
			&with_userinfo(&url, "alice"),
			checkout.to_str().unwrap(),
		],
		&[("GIT_ASKPASS", askpass.to_str().unwrap())],
	)
	.await;
	assert!(
		out.status.success(),
		"askpass clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(checkout.join("hello.txt").exists(), "checkout missing file");
}

/// After an authenticated clone (which saves the username), a later `fetch` re-authenticates: the
/// username comes from the saved remote and the password from `GIT_ASKPASS`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_reauthenticates_with_saved_username_and_askpass() {
	let work = unique_tmp("auth-fetch");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir.clone(), ServerHash::Sha256, "alice", "s3cr3t").await;

	// Clone with full userinfo (persists the `alice` username into remote.origin.url).
	let checkout = work.join("c");
	let out = gta_iso(
		&[
			"clone",
			&with_userinfo(&url, "alice:s3cr3t"),
			checkout.to_str().unwrap(),
		],
		&[],
	)
	.await;
	assert!(
		out.status.success(),
		"seed clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);

	// Advance the server, then fetch — the client must authenticate again, username from the saved
	// remote and password from askpass.
	commit_file(&git_dir, "more.txt", b"more\n").await;
	let askpass = work.join("askpass.sh");
	write_askpass(&askpass, "s3cr3t");
	let out = gta_iso(
		&["-C", checkout.to_str().unwrap(), "fetch"],
		&[("GIT_ASKPASS", askpass.to_str().unwrap())],
	)
	.await;
	assert!(
		out.status.success(),
		"authenticated fetch failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// A wrong password fails the clone: the server rejects it and — with no way to prompt
/// (`GIT_TERMINAL_PROMPT=0`, no askpass) — the credential provider declines, so the 401 stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_with_wrong_password_is_rejected() {
	let work = unique_tmp("auth-wrong");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir, ServerHash::Sha256, "alice", "s3cr3t").await;

	let checkout = work.join("c");
	let out = gta_iso(
		&[
			"clone",
			&with_userinfo(&url, "alice:wrong"),
			checkout.to_str().unwrap(),
		],
		&[("GIT_TERMINAL_PROMPT", "0")],
	)
	.await;
	assert!(
		!out.status.success(),
		"clone with a wrong password unexpectedly succeeded"
	);
	let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
	assert!(
		stderr.contains("401") || stderr.contains("auth"),
		"expected an auth failure, got: {stderr}"
	);
}

/// A configured credential helper supplies the credential before any prompt: with no URL userinfo, no
/// askpass, and terminal prompts disabled, the clone still authenticates from the helper's `get`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_supplies_credential_without_prompting() {
	let work = unique_tmp("auth-helper-get");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir, ServerHash::Sha256, "alice", "s3cr3t").await;

	// A helper whose store already holds the credential for this server, so its `get` supplies it.
	let helper = work.join("helper.sh");
	write_store_helper(&helper);
	let store = work.join("store");
	let host = url
		.strip_prefix("http://")
		.unwrap()
		.split('/')
		.next()
		.unwrap();
	std::fs::write(&store, format!("http://{host}\talice\ts3cr3t\n")).unwrap();
	let config = write_helper_config(&work, &[&helper]);

	let checkout = work.join("c");
	let out = gta_iso(
		&["clone", &url, checkout.to_str().unwrap()],
		&[
			("GIT_CONFIG_GLOBAL", config.to_str().unwrap()),
			("HELPER_FILE", store.to_str().unwrap()),
			("GIT_TERMINAL_PROMPT", "0"),
		],
	)
	.await;
	assert!(
		out.status.success(),
		"helper-authenticated clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(checkout.join("hello.txt").exists(), "checkout missing file");
}

/// `approve` persists an accepted credential and a later operation serves it: clone once from the URL
/// userinfo (which drives the helper's `store` on success), then fetch with no userinfo/askpass — the
/// helper's `get` returns the stored credential and the fetch authenticates.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_stores_then_a_later_get_serves() {
	let work = unique_tmp("auth-helper-store");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir.clone(), ServerHash::Sha256, "alice", "s3cr3t").await;

	let helper = work.join("helper.sh");
	write_store_helper(&helper);
	let store = work.join("store");
	let config = write_helper_config(&work, &[&helper]);
	let helper_env: [(&str, &str); 3] = [
		("GIT_CONFIG_GLOBAL", config.to_str().unwrap()),
		("HELPER_FILE", store.to_str().unwrap()),
		("GIT_TERMINAL_PROMPT", "0"),
	];

	// Clone with full userinfo: on success `approve` runs the helper's `store`.
	let checkout = work.join("c");
	let out = gta_iso(
		&[
			"clone",
			&with_userinfo(&url, "alice:s3cr3t"),
			checkout.to_str().unwrap(),
		],
		&helper_env,
	)
	.await;
	assert!(
		out.status.success(),
		"seed clone failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	let stored = std::fs::read_to_string(&store).unwrap_or_default();
	assert!(
		stored.contains("alice") && stored.contains("s3cr3t"),
		"approve did not store the credential: {stored:?}"
	);

	// Advance the server and fetch with no userinfo and no prompt: the helper's `get` must serve it.
	commit_file(&git_dir, "more.txt", b"more\n").await;
	let out = gta_iso(&["-C", checkout.to_str().unwrap(), "fetch"], &helper_env).await;
	assert!(
		out.status.success(),
		"fetch from stored credential failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// `reject` erases a stored-but-stale credential: after a credential is stored, a clone presenting a
/// *wrong* password is rejected by the server, and the client's `reject` runs the helper's `erase`,
/// removing the entry it keyed on.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reject_erases_the_stored_credential() {
	let work = unique_tmp("auth-helper-erase");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir, ServerHash::Sha256, "alice", "s3cr3t").await;

	let helper = work.join("helper.sh");
	write_store_helper(&helper);
	let store = work.join("store");
	let host = url
		.strip_prefix("http://")
		.unwrap()
		.split('/')
		.next()
		.unwrap();
	// Pre-seed a (now stale) stored credential for this server.
	std::fs::write(&store, format!("http://{host}\talice\tstale\n")).unwrap();
	let config = write_helper_config(&work, &[&helper]);

	// A clone that presents the stale userinfo password fails the 401, so the client rejects it.
	let checkout = work.join("c");
	let out = gta_iso(
		&[
			"clone",
			&with_userinfo(&url, "alice:stale"),
			checkout.to_str().unwrap(),
		],
		&[
			("GIT_CONFIG_GLOBAL", config.to_str().unwrap()),
			("HELPER_FILE", store.to_str().unwrap()),
			("GIT_TERMINAL_PROMPT", "0"),
		],
	)
	.await;
	assert!(
		!out.status.success(),
		"clone with a stale password unexpectedly succeeded"
	);
	let remaining = std::fs::read_to_string(&store).unwrap_or_default();
	assert!(
		!remaining.contains("stale"),
		"reject did not erase the stale credential: {remaining:?}"
	);
}

/// A helper chain feeds a learned username forward: the first helper supplies only the username, and
/// the second issues the password *only when it sees that username on its stdin* — so the clone
/// authenticates as `alice:s3cr3t` only if the get chain fed the username from the first helper to the
/// second, as git does.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_chain_feeds_a_learned_username_forward() {
	let work = unique_tmp("auth-helper-chain");
	let git_dir = work.join("srv.git");
	seed(&git_dir).await;
	let url = serve_gitana_basic_auth(git_dir, ServerHash::Sha256, "alice", "s3cr3t").await;

	// Helper 1: on `get`, supply only the username.
	let username_helper = work.join("username.sh");
	write_exec(
		&username_helper,
		"[ \"$1\" = get ] && echo username=alice\nexit 0\n",
	);
	// Helper 2: on `get`, supply the password *only if* the request it receives already carries
	// username=alice (i.e. the chain fed helper 1's username forward).
	let password_helper = work.join("password.sh");
	write_exec(
		&password_helper,
		"[ \"$1\" = get ] || exit 0\ngrep -q '^username=alice$' && echo password=s3cr3t\nexit 0\n",
	);
	let config = write_helper_config(&work, &[&username_helper, &password_helper]);

	let checkout = work.join("c");
	let out = gta_iso(
		&["clone", &url, checkout.to_str().unwrap()],
		&[
			("GIT_CONFIG_GLOBAL", config.to_str().unwrap()),
			("GIT_TERMINAL_PROMPT", "0"),
		],
	)
	.await;
	assert!(
		out.status.success(),
		"helper-chain clone failed (username was not fed forward?): {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(checkout.join("hello.txt").exists(), "checkout missing file");
}
