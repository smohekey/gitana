//! The credential-aware [`HttpTransport`]: git's 401-retry flow over a raw [`HttpClient`].

use std::sync::Mutex;

use anyhow::Result;

use crate::{
	Credential, CredentialProvider, CredentialRequest, Filled, HttpClient, HttpResponse,
	HttpTransport,
};

/// Pairs a raw [`HttpClient`] with a [`CredentialProvider`] and presents the body-returning
/// [`HttpTransport`] the porcelain consumes — putting git's HTTP credential flow in one place,
/// beneath both the CLI advertisement `GET` and the porcelain pack `POST`, so neither has to know
/// about auth.
///
/// Per request, matching git (curl with `CURLAUTH_ANY`): send **unauthenticated** first — never
/// disclosing a credential before the server asks for it — except that a credential already *accepted*
/// this operation is re-sent pre-emptively (so a clone/fetch/push whose advertisement `GET`
/// authenticates does not re-challenge on its pack `POST`). On a `401`, resolve a credential — the URL
/// userinfo one first (Basic, only against a Basic offer), then [`CredentialProvider::fill`], which
/// reads the challenge to pick a scheme: Basic, or (under git's `authtype` capability)
/// Bearer/Digest/…. Retry, and report the outcome (`approve` on success, `reject` on a repeat `401`). A
/// credential the provider flags as a non-final **multistage** step (git's `continue`, for
/// NTLM/Kerberos) drives git's `HTTP_REAUTH` loop — re-fill with the fresh challenge, carrying the prior
/// round's credential context (git clears only the secret, retaining `username`/`authtype`/`ephemeral`/
/// `state[]`), capped at [`MAX_AUTH_ROUNDS`]. An accepted credential — `ephemeral` or not — is cached and
/// re-sent for the rest of the operation, as git reuses its `http_auth`. A `401` the provider cannot
/// fill, and any other non-2xx, is an error.
///
/// Cross-host **redirect** following for auth is deliberately not handled here (reqwest strips
/// `Authorization` across origins, so a redirected repository simply fails to authenticate rather than
/// leaking a credential); it is deferred to a later slice.
///
/// `username`/`password` seed the flow from URL userinfo ([`Origin`](crate::Origin) strips it off the
/// request URLs): a full `user:pass` becomes the first credential tried on a Basic challenge (still not
/// sent before one), and a bare username becomes the [`CredentialRequest`] hint so a helper/prompt is
/// pre-filled.
pub struct AuthTransport<C, P> {
	client: C,
	provider: P,
	/// The repository base URL (no userinfo, no service endpoint) the [`CredentialRequest`] is keyed on
	/// — so a path-sensitive lookup sees git's `acme/app.git`, not `acme/app.git/info/refs`.
	base_url: String,
	/// A known username to key the credential request on (URL userinfo, else `None` — the provider may
	/// still supply one from config).
	username_hint: Option<String>,
	/// A complete URL-userinfo credential, tried **first** on a Basic `401` (before prompting), and taken
	/// once so a later challenge falls through to [`CredentialProvider::fill`]. Never sent pre-emptively.
	url_credential: Mutex<Option<Credential>>,
	/// A credential the server has **accepted** this operation, with its final multistage `state[]`,
	/// re-sent pre-emptively on later requests (and already approved when cached). The state rides along so
	/// a later request that gets a `401` can `reject` the credential with the same state a stateful helper
	/// keyed it on. `None` until an auth succeeds. Interior-mutable because [`HttpTransport`] takes `&self`;
	/// the guard is never held across an `.await`.
	cached: Mutex<Option<(Credential, Vec<String>)>>,
}

/// One raw request, so the retry loop can re-issue it verbatim with different headers.
enum Request<'a> {
	Get {
		url: &'a str,
	},
	Post {
		url: &'a str,
		content_type: &'a str,
		body: &'a [u8],
	},
}

impl Request<'_> {
	fn url(&self) -> &str {
		match self {
			Request::Get { url } | Request::Post { url, .. } => url,
		}
	}
}

impl<C: HttpClient, P: CredentialProvider> AuthTransport<C, P> {
	/// Wrap `client` with `provider`, keying credentials on `base_url` with no userinfo hint —
	/// credentials come entirely from the provider (config / helper / prompt).
	pub fn new(client: C, provider: P, base_url: String) -> Self {
		Self::with_userinfo(client, provider, base_url, None, None)
	}

	/// Wrap `client` with `provider`, keying credentials on `base_url` (the repository URL, no userinfo
	/// or endpoint) and seeding from URL userinfo: a full `user:pass` becomes the first credential tried
	/// on a Basic challenge; `Some(user)` alone is only a username hint; both `None` is [`new`](Self::new).
	pub fn with_userinfo(
		client: C,
		provider: P,
		base_url: String,
		username: Option<String>,
		password: Option<String>,
	) -> Self {
		// A full userinfo credential is a candidate for the challenge retry — never sent pre-emptively.
		let url_credential = match (&username, password) {
			(Some(username), Some(password)) => Some(Credential::basic(username.clone(), password)),
			_ => None,
		};
		Self {
			client,
			provider,
			base_url,
			username_hint: username,
			url_credential: Mutex::new(url_credential),
			cached: Mutex::new(None),
		}
	}

	/// Report an accepted `credential` (with its final multistage `state[]`) to the provider
	/// (best-effort). A `base_url` with no keyable host simply skips the callback.
	async fn note_approved(&self, credential: &Credential, state: &[String]) {
		if let Some(request) = self.callback_request(state) {
			let _ = self.provider.approve(&request, credential).await;
		}
	}

	/// Report a rejected `credential` (with its final `state[]`) to the provider (best-effort); see
	/// [`note_approved`](Self::note_approved).
	async fn note_rejected(&self, credential: &Credential, state: &[String]) {
		if let Some(request) = self.callback_request(state) {
			let _ = self.provider.reject(&request, credential).await;
		}
	}

	/// The credential request for the `approve`/`reject` callbacks — keyed exactly as the `fill` request
	/// (the repository base URL plus the URL-userinfo username *hint*), **not** the credential's finally
	/// resolved username. git settles which helpers/`useHttpPath` apply once, during fill, from the
	/// pre-helper attributes; keying the callbacks the same way makes `approve`/`reject` run that same
	/// helper chain (a username a helper *learned* must not retroactively enable a username-qualified
	/// `credential.<user>@host` section that fill never consulted). The credential's own username is still
	/// what the provider *writes* to each helper.
	fn callback_request(&self, state: &[String]) -> Option<CredentialRequest> {
		CredentialRequest::from_url(&self.base_url, self.username_hint.clone())
			.map(|request| request.with_state(state.to_vec()))
	}

	/// Issue `request` once with `headers`.
	async fn send(
		&self,
		request: &Request<'_>,
		headers: &[(String, String)],
	) -> Result<HttpResponse> {
		match request {
			Request::Get { url } => self.client.get(url, headers).await,
			Request::Post {
				url,
				content_type,
				body,
			} => {
				self
					.client
					.post(url, content_type, body.to_vec(), headers)
					.await
			}
		}
	}

	/// Run `request` through git's credential flow and return the 2xx body (or the same non-2xx error
	/// the raw transports used to raise).
	async fn run(&self, request: Request<'_>) -> Result<Vec<u8>> {
		let url = request.url();

		// First attempt: unauthenticated, unless a credential accepted earlier this operation is in force —
		// git never sends a credential before a challenge, but does re-send an accepted one pre-emptively.
		let attached = self.cached.lock().expect("cache not poisoned").clone();
		let headers = attached
			.as_ref()
			.map(|(credential, _)| auth_headers(credential))
			.unwrap_or_default();
		let mut response = self.send(&request, &headers).await?;
		if response.status != 401 {
			// Success or a non-auth failure — nothing to negotiate (an accepted credential was approved when
			// it was cached).
			return response.into_body(url);
		}

		// A `401`. If we attached the cached credential and it was rejected, it is stale — drop and reject
		// it on **any** `401` (even one that no longer offers Basic), so a helper never retains a
		// known-bad secret. `reject`/`approve` are best-effort (the trait's contract) — a helper failing to
		// record the outcome must not fail the operation, so their errors are dropped.
		if let Some((stale, stale_state)) = attached {
			self.note_rejected(&stale, &stale_state).await;
			*self.cached.lock().expect("cache not poisoned") = None;
		}

		// Try the URL-userinfo credential first (taken once), but only against a challenge that offers
		// Basic — gitana never sends Basic the server did not ask for. On its own `401`, **adopt that
		// response** so the subsequent provider fill sees the server's *latest* challenge (a new realm, or
		// a switch away from Basic), matching git's re-read of `WWW-Authenticate` on each rejection.
		// Take the URL-userinfo credential out from under its lock *before* any `.await` — the guard is a
		// `std::sync::Mutex`, which must never be held across an await point.
		let url_credential = response
			.offers_basic_auth()
			.then(|| {
				self
					.url_credential
					.lock()
					.expect("cache not poisoned")
					.take()
			})
			.flatten();
		if let Some(url_credential) = url_credential {
			let retry = self.send(&request, &auth_headers(&url_credential)).await?;
			if retry.status == 401 {
				self.note_rejected(&url_credential, &[]).await;
				response = retry;
			} else {
				if retry.is_success() {
					self.note_approved(&url_credential, &[]).await;
					self.cache(url_credential, Vec::new());
				}
				return retry.into_body(url);
			}
		}

		// Negotiated resolution. Fill from the provider, which reads the `401`'s challenge to pick a scheme
		// — Basic, or (under git's `authtype` capability) Bearer/Digest/… — then retry. On a further `401`
		// the provider flagged as a non-final **multistage** step (git's `continue`, for NTLM/Kerberos),
		// re-fill with the new challenge and the returned `state[]` and loop; otherwise the credential was
		// rejected — erase it and let the `401` stand. Unlike Basic's single fill+retry (git's
		// `HTTP_NOAUTH`), a multistage credential drives git's `HTTP_REAUTH` loop, capped here at
		// [`MAX_AUTH_ROUNDS`] so a buggy helper cannot spin forever. The provider fills from the repository
		// *base* URL (not this service endpoint) so a path-sensitive lookup matches git's.
		let Some(base_request) =
			CredentialRequest::from_url(&self.base_url, self.username_hint.clone())
		else {
			return response.into_body(url);
		};
		let mut state: Vec<String> = Vec::new();
		// The prior round's fill — its non-secret context (username/authtype/ephemeral) and negotiated
		// capabilities are re-presented, exactly as git retains those and clears only the secret between
		// rounds. The secret itself is never re-presented (only the context is read off it).
		let mut carried: Option<Filled> = None;
		for _ in 0..MAX_AUTH_ROUNDS {
			let mut cred_request = base_request
				.clone()
				.with_wwwauth(response.www_authenticate.clone())
				.with_state(state.clone());
			if let Some(previous) = &carried {
				cred_request = cred_request.with_credential_context(previous);
			}
			let Some(filled) = self.provider.fill(&cred_request).await? else {
				// No credential for this challenge (no helper match, and no interactive Basic prompt because
				// the server did not offer Basic) — let the server's `401` stand. gitana resolves once per
				// operation (no refill loop, which git lacks and which could spin on a helper minting secrets).
				return response.into_body(url);
			};
			// git negotiates the scheme from the challenge (curl's `CURLAUTH_ANY`): never send a *Basic*
			// credential — a base64 secret — to a server that did not offer Basic, even if a legacy helper or
			// a Basic-only source returned one. An encoded credential's scheme was picked by the helper from
			// the challenge, so it is sent as-is.
			if filled.credential.is_basic() && !response.offers_basic_auth() {
				return response.into_body(url);
			}
			let retry = self
				.send(&request, &auth_headers(&filled.credential))
				.await?;
			if retry.status != 401 {
				// Report the outcome with the credential's final `state[]` — git forwards it to `store`/`erase`
				// so a stateful helper can persist or clean up the negotiated credential.
				if retry.is_success() {
					self.note_approved(&filled.credential, &filled.state).await;
					self.cache(filled.credential, filled.state);
				}
				return retry.into_body(url);
			}
			// A further `401`. If the helper expects another multistage round, this is a continuation (not a
			// rejection): carry its `state[]` and the fresh challenge into the next fill. Otherwise the
			// credential failed — erase it and stop.
			if filled.more {
				state = filled.state.clone();
				carried = Some(filled);
				response = retry;
				continue;
			}
			self.note_rejected(&filled.credential, &filled.state).await;
			return retry.into_body(url);
		}
		// Exhausted the multistage round cap without resolving — let the last `401` stand.
		response.into_body(url)
	}

	/// Cache `credential` (with its final `state[]`) for pre-emptive reuse on later requests this
	/// operation — git keeps the accepted credential in its in-memory `http_auth` and re-sends it on every
	/// subsequent request of the operation. This holds even for an `ephemeral` credential: `ephemeral`
	/// tells a *helper* not to persist the secret to disk (honoured by forwarding `ephemeral=1` on `store`),
	/// but git still reuses the value for the rest of the operation. Dropping it here would force each later
	/// request through a fresh `401`/fill and fail with a one-shot helper that cannot reissue the token.
	fn cache(&self, credential: Credential, state: Vec<String>) {
		*self.cached.lock().expect("cache not poisoned") = Some((credential, state));
	}
}

/// The multistage authentication round cap (git's `HTTP_REAUTH` loop): NTLM is 2 rounds, Kerberos a few
/// more; beyond this a helper is misbehaving, so the last `401` stands rather than looping forever.
const MAX_AUTH_ROUNDS: usize = 5;

/// The `Authorization` header for `credential` (`Basic …` or `<authtype> …`), as a one-element list.
fn auth_headers(credential: &Credential) -> Vec<(String, String)> {
	vec![("Authorization".to_owned(), credential.auth_header())]
}

impl<C: HttpClient, P: CredentialProvider> HttpTransport for AuthTransport<C, P> {
	async fn get(&self, url: &str) -> Result<Vec<u8>> {
		self.run(Request::Get { url }).await
	}

	async fn post(&self, url: &str, content_type: &str, body: Vec<u8>) -> Result<Vec<u8>> {
		self
			.run(Request::Post {
				url,
				content_type,
				body: &body,
			})
			.await
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use super::*;
	use crate::Filled;

	/// A client that requires HTTP Basic with a specific `Authorization` value: it answers `200` only
	/// when that exact header is present, else `401` offering Basic. Records every `Authorization` value
	/// it is asked to send (`None` = unauthenticated).
	struct AuthRequiredClient {
		expected: String,
		log: Arc<std::sync::Mutex<Vec<Option<String>>>>,
	}

	impl AuthRequiredClient {
		fn answer(&self, headers: &[(String, String)]) -> HttpResponse {
			let auth = headers
				.iter()
				.find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
				.map(|(_, value)| value.clone());
			self.log.lock().unwrap().push(auth.clone());
			if auth.as_deref() == Some(self.expected.as_str()) {
				HttpResponse {
					status: 200,
					www_authenticate: Vec::new(),
					body: b"ok".to_vec(),
				}
			} else {
				HttpResponse {
					status: 401,
					www_authenticate: vec!["Basic realm=\"x\"".to_owned()],
					body: Vec::new(),
				}
			}
		}
	}

	impl HttpClient for AuthRequiredClient {
		async fn get(&self, _url: &str, headers: &[(String, String)]) -> Result<HttpResponse> {
			Ok(self.answer(headers))
		}

		async fn post(
			&self,
			_url: &str,
			_content_type: &str,
			_body: Vec<u8>,
			headers: &[(String, String)],
		) -> Result<HttpResponse> {
			Ok(self.answer(headers))
		}
	}

	/// A provider that fills a fixed credential.
	struct FixedProvider {
		username: String,
		password: String,
	}

	impl CredentialProvider for FixedProvider {
		async fn fill(&self, _request: &CredentialRequest) -> Result<Option<Filled>> {
			Ok(Some(Filled::once(Credential::basic(
				self.username.clone(),
				self.password.clone(),
			))))
		}

		async fn approve(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}

		async fn reject(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}
	}

	/// A provider that panics if asked to fill — proves a path resolves without consulting it.
	struct PanicProvider;

	impl CredentialProvider for PanicProvider {
		async fn fill(&self, _request: &CredentialRequest) -> Result<Option<Filled>> {
			panic!("provider fill must not be called")
		}

		async fn approve(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}

		async fn reject(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}
	}

	fn header_for(username: &str, password: &str) -> String {
		Credential::basic(username.to_owned(), password.to_owned()).auth_header()
	}

	#[tokio::test]
	async fn authenticates_on_challenge_then_reuses_the_credential() {
		let base = "https://example.com/acme/app.git";
		let log = Arc::new(std::sync::Mutex::new(Vec::new()));
		let transport = AuthTransport::new(
			AuthRequiredClient {
				expected: header_for("user", "pass"),
				log: log.clone(),
			},
			FixedProvider {
				username: "user".to_owned(),
				password: "pass".to_owned(),
			},
			base.to_owned(),
		);

		// The advertisement GET is unauthenticated, gets a Basic 401, fills, retries, and succeeds.
		let body = transport
			.get(&format!("{base}/info/refs?service=git-upload-pack"))
			.await
			.unwrap();
		assert_eq!(body, b"ok");
		// The pack POST re-sends the accepted credential pre-emptively — no fresh challenge round-trip.
		let body = transport
			.post(&format!("{base}/git-upload-pack"), "ct", vec![1, 2, 3])
			.await
			.unwrap();
		assert_eq!(body, b"ok");

		let log = log.lock().unwrap();
		// First request: unauth then authed retry. Second request: authed pre-emptively on the first send.
		assert_eq!(log[0], None);
		assert_eq!(log[1].as_deref(), Some(header_for("user", "pass").as_str()));
		assert_eq!(
			log.last().unwrap().as_deref(),
			Some(header_for("user", "pass").as_str())
		);
	}

	#[tokio::test]
	async fn prefers_the_url_credential_over_the_provider() {
		let base = "https://example.com/acme/app.git";
		let log = Arc::new(std::sync::Mutex::new(Vec::new()));
		// The URL userinfo credential is the one the server accepts; the provider must never be consulted.
		let transport = AuthTransport::with_userinfo(
			AuthRequiredClient {
				expected: header_for("alice", "s3cr3t"),
				log: log.clone(),
			},
			PanicProvider,
			base.to_owned(),
			Some("alice".to_owned()),
			Some("s3cr3t".to_owned()),
		);

		let body = transport
			.get(&format!("{base}/info/refs?service=git-upload-pack"))
			.await
			.unwrap();
		assert_eq!(body, b"ok");
		// The first send was unauthenticated (never pre-emptive), the retry used the URL credential.
		let log = log.lock().unwrap();
		assert_eq!(log[0], None);
		assert_eq!(
			log[1].as_deref(),
			Some(header_for("alice", "s3cr3t").as_str())
		);
	}

	/// A client whose challenge *shifts*: the unauthenticated request gets a Basic `401`, but once a
	/// (wrong) credential is presented the server answers `401` offering only Bearer. Records each
	/// `Authorization` value it saw.
	struct ShiftingChallengeClient {
		log: Arc<std::sync::Mutex<Vec<Option<String>>>>,
	}

	impl ShiftingChallengeClient {
		fn answer(&self, headers: &[(String, String)]) -> HttpResponse {
			let auth = headers
				.iter()
				.find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
				.map(|(_, value)| value.clone());
			self.log.lock().unwrap().push(auth.clone());
			let challenge = if auth.is_none() {
				"Basic realm=\"x\""
			} else {
				// Once a credential is offered, the server no longer accepts Basic.
				"Bearer realm=\"y\""
			};
			HttpResponse {
				status: 401,
				www_authenticate: vec![challenge.to_owned()],
				body: Vec::new(),
			}
		}
	}

	impl HttpClient for ShiftingChallengeClient {
		async fn get(&self, _url: &str, headers: &[(String, String)]) -> Result<HttpResponse> {
			Ok(self.answer(headers))
		}

		async fn post(
			&self,
			_url: &str,
			_content_type: &str,
			_body: Vec<u8>,
			headers: &[(String, String)],
		) -> Result<HttpResponse> {
			Ok(self.answer(headers))
		}
	}

	/// A provider that declines (fills nothing) — a helper with no credential for the challenge, and no
	/// interactive prompt (e.g. the server offered only a scheme it cannot supply).
	struct DeclineProvider;

	impl CredentialProvider for DeclineProvider {
		async fn fill(&self, _request: &CredentialRequest) -> Result<Option<Filled>> {
			Ok(None)
		}

		async fn approve(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}

		async fn reject(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}
	}

	#[tokio::test]
	async fn a_shifted_bearer_challenge_the_provider_cannot_fill_stops_cleanly() {
		let base = "https://example.com/acme/app.git";
		let log = Arc::new(std::sync::Mutex::new(Vec::new()));
		// The URL credential is wrong; its rejection shifts the challenge to Bearer-only. The provider is
		// now consulted for that challenge (a helper *could* return a Bearer token), but it declines, so
		// no Bearer/Basic credential is sent and the `401` stands.
		let transport = AuthTransport::with_userinfo(
			ShiftingChallengeClient { log: log.clone() },
			DeclineProvider,
			base.to_owned(),
			Some("alice".to_owned()),
			Some("wrong".to_owned()),
		);

		let result = transport
			.get(&format!("{base}/info/refs?service=git-upload-pack"))
			.await;
		// The server's 401 stands as an error; the provider declined, so nothing more was sent.
		assert!(result.is_err());
		let log = log.lock().unwrap();
		// Exactly two sends: unauthenticated, then the URL credential — the declined fill sent nothing more.
		assert_eq!(log.len(), 2);
		assert_eq!(log[0], None);
		assert_eq!(
			log[1].as_deref(),
			Some(header_for("alice", "wrong").as_str())
		);
	}

	/// A provider that fills a fixed credential and records every credential it is asked to `reject`.
	struct RejectRecordingProvider {
		username: String,
		password: String,
		rejected: Arc<std::sync::Mutex<Vec<Credential>>>,
	}

	impl CredentialProvider for RejectRecordingProvider {
		async fn fill(&self, _request: &CredentialRequest) -> Result<Option<Filled>> {
			Ok(Some(Filled::once(Credential::basic(
				self.username.clone(),
				self.password.clone(),
			))))
		}

		async fn approve(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}

		async fn reject(&self, _request: &CredentialRequest, cred: &Credential) -> Result<()> {
			self.rejected.lock().unwrap().push(cred.clone());
			Ok(())
		}
	}

	#[tokio::test]
	async fn a_rejected_filled_credential_is_erased_and_the_401_stands() {
		let base = "https://example.com/acme/app.git";
		let log = Arc::new(std::sync::Mutex::new(Vec::new()));
		let rejected = Arc::new(std::sync::Mutex::new(Vec::new()));
		// The provider fills a wrong credential the server never accepts. git does a single fill+retry for
		// Basic, then gives up (`handle_curl_result` returns `HTTP_NOAUTH`, not `HTTP_REAUTH`): the
		// credential is erased (rejected) exactly once and the 401 stands — there is no refill loop.
		let transport = AuthTransport::new(
			AuthRequiredClient {
				expected: header_for("alice", "right"),
				log: log.clone(),
			},
			RejectRecordingProvider {
				username: "alice".to_owned(),
				password: "wrong".to_owned(),
				rejected: rejected.clone(),
			},
			base.to_owned(),
		);

		let result = transport
			.get(&format!("{base}/info/refs?service=git-upload-pack"))
			.await;
		assert!(result.is_err(), "the server's 401 should stand");
		// Exactly one rejection (erase), then done — no re-fill, no loop.
		assert_eq!(rejected.lock().unwrap().len(), 1);
		assert_eq!(
			rejected.lock().unwrap()[0],
			Credential::basic("alice".to_owned(), "wrong".to_owned())
		);
		// Two sends: unauthenticated, then the single filled credential.
		assert_eq!(log.lock().unwrap().len(), 2);
	}

	/// A server that authenticates a single exact `Authorization` header, else `401` offering `scheme`.
	struct TokenClient {
		expected: String,
		scheme: &'static str,
		log: Arc<std::sync::Mutex<Vec<Option<String>>>>,
	}

	impl TokenClient {
		fn answer(&self, headers: &[(String, String)]) -> HttpResponse {
			let auth = headers
				.iter()
				.find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
				.map(|(_, value)| value.clone());
			self.log.lock().unwrap().push(auth.clone());
			if auth.as_deref() == Some(self.expected.as_str()) {
				HttpResponse {
					status: 200,
					www_authenticate: Vec::new(),
					body: b"ok".to_vec(),
				}
			} else {
				HttpResponse {
					status: 401,
					www_authenticate: vec![format!("{} realm=\"x\"", self.scheme)],
					body: Vec::new(),
				}
			}
		}
	}

	impl HttpClient for TokenClient {
		async fn get(&self, _url: &str, headers: &[(String, String)]) -> Result<HttpResponse> {
			Ok(self.answer(headers))
		}

		async fn post(
			&self,
			_url: &str,
			_content_type: &str,
			_body: Vec<u8>,
			headers: &[(String, String)],
		) -> Result<HttpResponse> {
			Ok(self.answer(headers))
		}
	}

	/// A provider that fills a fixed [`Filled`] (used for the Bearer/`authtype` path).
	struct EncodedProvider(Filled);

	impl CredentialProvider for EncodedProvider {
		async fn fill(&self, _request: &CredentialRequest) -> Result<Option<Filled>> {
			Ok(Some(self.0.clone()))
		}

		async fn approve(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}

		async fn reject(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}
	}

	#[tokio::test]
	async fn authenticates_with_a_bearer_token_from_the_provider() {
		// The provider returns git's `authtype`/`credential` form; the transport sends `Bearer <token>`.
		let base = "https://example.com/acme/app.git";
		let log = Arc::new(std::sync::Mutex::new(Vec::new()));
		let transport = AuthTransport::new(
			TokenClient {
				expected: "Bearer tok.en".to_owned(),
				scheme: "Bearer",
				log: log.clone(),
			},
			EncodedProvider(Filled::once(Credential::encoded(
				"Bearer".to_owned(),
				"tok.en".to_owned(),
			))),
			base.to_owned(),
		);

		let body = transport
			.get(&format!("{base}/info/refs?service=git-upload-pack"))
			.await
			.unwrap();
		assert_eq!(body, b"ok");
		let log = log.lock().unwrap();
		assert_eq!(log[0], None);
		assert_eq!(log[1].as_deref(), Some("Bearer tok.en"));
	}

	/// A two-round multistage server: the first token gets a `401` continuation, the second succeeds.
	struct MultistageClient {
		round1: String,
		round2: String,
		log: Arc<std::sync::Mutex<Vec<Option<String>>>>,
	}

	impl MultistageClient {
		fn answer(&self, headers: &[(String, String)]) -> HttpResponse {
			let auth = headers
				.iter()
				.find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
				.map(|(_, value)| value.clone());
			self.log.lock().unwrap().push(auth.clone());
			match auth.as_deref() {
				Some(a) if a == self.round2 => HttpResponse {
					status: 200,
					www_authenticate: Vec::new(),
					body: b"ok".to_vec(),
				},
				// Unauthenticated, or the first-round token: answer 401 with a (continuation) challenge.
				Some(a) if a == self.round1 => HttpResponse {
					status: 401,
					www_authenticate: vec!["Negotiate stage2".to_owned()],
					body: Vec::new(),
				},
				_ => HttpResponse {
					status: 401,
					www_authenticate: vec!["Negotiate".to_owned()],
					body: Vec::new(),
				},
			}
		}
	}

	impl HttpClient for MultistageClient {
		async fn get(&self, _url: &str, headers: &[(String, String)]) -> Result<HttpResponse> {
			Ok(self.answer(headers))
		}

		async fn post(
			&self,
			_url: &str,
			_content_type: &str,
			_body: Vec<u8>,
			headers: &[(String, String)],
		) -> Result<HttpResponse> {
			Ok(self.answer(headers))
		}
	}

	/// A provider driving a two-round handshake: round 1 (no prior state) returns a token and asks to
	/// continue with a `state[]`; round 2 (seeing that state echoed back) returns the finalizing token.
	/// Records the carried context (`authtype`, `ephemeral`) each `fill` request arrived with, so a test can
	/// assert the transport re-presents the prior round's context (git retains it across `HTTP_REAUTH`).
	#[derive(Default)]
	struct MultistageProvider {
		seen: Arc<std::sync::Mutex<Vec<SeenContext>>>,
	}

	/// The carried context a `fill` request arrived with (for asserting the transport re-presents it).
	#[derive(Debug, PartialEq, Eq)]
	struct SeenContext {
		authtype: Option<String>,
		ephemeral: bool,
		carried_username: Option<String>,
		caps_authtype: bool,
		caps_state: bool,
	}

	impl CredentialProvider for MultistageProvider {
		async fn fill(&self, request: &CredentialRequest) -> Result<Option<Filled>> {
			self.seen.lock().unwrap().push(SeenContext {
				authtype: request.authtype.clone(),
				ephemeral: request.ephemeral,
				carried_username: request.carried_username.clone(),
				caps_authtype: request.caps_authtype,
				caps_state: request.caps_state,
			});
			let encoded = |credential: &str| Credential {
				// Round one learns an account name; the transport must re-present it next round.
				username: Some("alice".to_owned()),
				authtype: Some("Negotiate".to_owned()),
				credential: Some(credential.to_owned()),
				ephemeral: true,
				..Credential::default()
			};
			if request.state.iter().any(|s| s == "helper:stage1") {
				Ok(Some(Filled::once(encoded("round2"))))
			} else {
				Ok(Some(Filled {
					credential: encoded("round1"),
					state: vec!["helper:stage1".to_owned()],
					more: true,
					// Round one negotiated both capabilities; the transport must carry each into round two.
					caps_authtype: true,
					caps_state: true,
				}))
			}
		}

		async fn approve(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}

		async fn reject(&self, _request: &CredentialRequest, _cred: &Credential) -> Result<()> {
			Ok(())
		}
	}

	#[tokio::test]
	async fn a_basic_credential_is_not_sent_to_a_bearer_only_server() {
		// A provider (helper/store) may return a Basic credential; it must never be base64-sent to a server
		// that offered only Bearer — the scheme is negotiated from the challenge.
		let base = "https://example.com/acme/app.git";
		let log = Arc::new(std::sync::Mutex::new(Vec::new()));
		let transport = AuthTransport::new(
			TokenClient {
				expected: "unused".to_owned(),
				scheme: "Bearer",
				log: log.clone(),
			},
			FixedProvider {
				username: "user".to_owned(),
				password: "pass".to_owned(),
			},
			base.to_owned(),
		);
		let result = transport
			.get(&format!("{base}/info/refs?service=git-upload-pack"))
			.await;
		assert!(result.is_err(), "the Bearer-only 401 should stand");
		// Only the unauthenticated send — the Basic credential was withheld from the Bearer challenge.
		let log = log.lock().unwrap();
		assert_eq!(log.len(), 1);
		assert_eq!(log[0], None);
	}

	#[tokio::test]
	async fn an_encoded_basic_credential_is_not_sent_to_a_bearer_only_server() {
		// A capability-aware helper may return the Basic secret pre-encoded (`authtype=basic`); it is still
		// a Basic credential and must not be sent to a server that offered only Bearer.
		let base = "https://example.com/acme/app.git";
		let log = Arc::new(std::sync::Mutex::new(Vec::new()));
		let transport = AuthTransport::new(
			TokenClient {
				expected: "unused".to_owned(),
				scheme: "Bearer",
				log: log.clone(),
			},
			EncodedProvider(Filled::once(Credential::encoded(
				"basic".to_owned(),
				"dXNlcjpwYXNz".to_owned(),
			))),
			base.to_owned(),
		);
		let result = transport
			.get(&format!("{base}/info/refs?service=git-upload-pack"))
			.await;
		assert!(result.is_err(), "the Bearer-only 401 should stand");
		// Only the unauthenticated send — the encoded Basic credential was withheld.
		assert_eq!(log.lock().unwrap().len(), 1);
	}

	#[tokio::test]
	async fn drives_a_multistage_handshake_with_state() {
		// git's `HTTP_REAUTH` loop: a first-round token gets a continuation `401`, and the provider — fed
		// the new challenge and its echoed `state[]` — supplies the finalizing token that succeeds.
		let base = "https://example.com/acme/app.git";
		let log = Arc::new(std::sync::Mutex::new(Vec::new()));
		let provider = MultistageProvider::default();
		let seen = provider.seen.clone();
		let transport = AuthTransport::new(
			MultistageClient {
				round1: "Negotiate round1".to_owned(),
				round2: "Negotiate round2".to_owned(),
				log: log.clone(),
			},
			provider,
			base.to_owned(),
		);

		let body = transport
			.get(&format!("{base}/info/refs?service=git-upload-pack"))
			.await
			.unwrap();
		assert_eq!(body, b"ok");
		let log = log.lock().unwrap();
		// Unauth, round-1 token (continuation 401), round-2 token (success) — three sends.
		assert_eq!(log[0], None);
		assert_eq!(log[1].as_deref(), Some("Negotiate round1"));
		assert_eq!(log[2].as_deref(), Some("Negotiate round2"));
		assert_eq!(log.len(), 3);
		// git retains the credential context across the round (clearing only the secret): the round-2 fill
		// re-presents round 1's `authtype`/`ephemeral` and the username round 1 learned, so a continuation
		// helper resumes the same scheme/account and the ephemeral marker is not lost. Round 1 arrives with
		// no carried context.
		let seen = seen.lock().unwrap();
		assert_eq!(
			seen[0],
			SeenContext {
				authtype: None,
				ephemeral: false,
				carried_username: None,
				caps_authtype: false,
				caps_state: false,
			}
		);
		assert_eq!(
			seen[1],
			SeenContext {
				authtype: Some("Negotiate".to_owned()),
				ephemeral: true,
				carried_username: Some("alice".to_owned()),
				caps_authtype: true,
				caps_state: true,
			}
		);
		assert_eq!(seen.len(), 2);
	}
}
