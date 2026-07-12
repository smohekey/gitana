//! The credential-aware [`HttpTransport`]: git's 401-retry flow over a raw [`HttpClient`].

use std::sync::Mutex;

use anyhow::Result;

use crate::{
	Credential, CredentialProvider, CredentialRequest, HttpClient, HttpResponse, HttpTransport,
};

/// Pairs a raw [`HttpClient`] with a [`CredentialProvider`] and presents the body-returning
/// [`HttpTransport`] the porcelain consumes — putting git's HTTP credential flow in one place,
/// beneath both the CLI advertisement `GET` and the porcelain pack `POST`, so neither has to know
/// about auth.
///
/// Per request, matching git (curl with `CURLAUTH_ANY`): send **unauthenticated** first — never
/// disclosing a credential before the server asks for it — except that a credential already *accepted*
/// this operation is re-sent pre-emptively (so a clone/fetch/push whose advertisement `GET`
/// authenticates does not re-challenge on its pack `POST`). On a `401` that offers Basic, resolve a
/// credential — the URL userinfo one first, then [`CredentialProvider::fill`] — retry, and report the
/// outcome (`approve` on success, `reject` on a repeat `401`). A `401` that does not offer Basic, and
/// any other non-2xx, is an error, as before.
///
/// Cross-host **redirect** following for auth is deliberately not handled here (reqwest strips
/// `Authorization` across origins, so a redirected repository simply fails to authenticate rather than
/// leaking a credential); it is deferred to the URL-rewriting slice.
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
	/// A credential the server has **accepted** this operation, re-sent pre-emptively on later requests
	/// (and already approved when cached). `None` until an auth succeeds. Interior-mutable because
	/// [`HttpTransport`] takes `&self`; the guard is never held across an `.await`.
	cached: Mutex<Option<Credential>>,
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
			(Some(username), Some(password)) => Some(Credential {
				username: username.clone(),
				password,
			}),
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

	/// Report an accepted `credential` to the provider (best-effort). A `base_url` with no keyable host
	/// simply skips the callback.
	async fn note_approved(&self, credential: &Credential) {
		if let Some(request) = self.callback_request() {
			let _ = self.provider.approve(&request, credential).await;
		}
	}

	/// Report a rejected `credential` to the provider (best-effort); see [`note_approved`](Self::note_approved).
	async fn note_rejected(&self, credential: &Credential) {
		if let Some(request) = self.callback_request() {
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
	fn callback_request(&self) -> Option<CredentialRequest> {
		CredentialRequest::from_url(&self.base_url, self.username_hint.clone())
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
		let headers = attached.as_ref().map(auth_headers).unwrap_or_default();
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
		if let Some(stale) = attached {
			self.note_rejected(&stale).await;
			*self.cached.lock().expect("cache not poisoned") = None;
		}

		// Try the URL-userinfo credential first (taken once), but only against a challenge that offers
		// Basic — gitana never sends Basic the server did not ask for. On its own `401`, **adopt that
		// response** so the subsequent provider fill sees the server's *latest* challenge (a new realm, or
		// a switch away from Basic), matching git's re-read of `WWW-Authenticate` on each rejection.
		if response.offers_basic_auth()
			&& let Some(url_credential) = self
				.url_credential
				.lock()
				.expect("cache not poisoned")
				.take()
		{
			let retry = self.send(&request, &auth_headers(&url_credential)).await?;
			if retry.status == 401 {
				self.note_rejected(&url_credential).await;
				response = retry;
			} else {
				if retry.is_success() {
					self.note_approved(&url_credential).await;
					*self.cached.lock().expect("cache not poisoned") = Some(url_credential);
				}
				return retry.into_body(url);
			}
		}

		if !response.offers_basic_auth() {
			// The current `401` does not offer Basic (Bearer/Negotiate, or no challenge at all): gitana
			// speaks only Basic, so do not prompt for or transmit Basic credentials the server did not ask
			// for — let the server's `401` stand.
			return response.into_body(url);
		}

		// Fill from the provider once and retry. git does a single fill+retry for Basic: on a rejecting
		// `401` its `handle_curl_result` returns `HTTP_NOAUTH` (not `HTTP_REAUTH`, which is reserved for
		// multistage NTLM/Kerberos), so it does **not** re-enter resolution within one operation — it
		// erases the credential and fails, and a *rerun* then finds the helper empty and prompts. So a
		// stale helper credential is rejected (erased) here and the `401` stands; there is deliberately no
		// refill loop (which git lacks, and which could spin on a helper minting fresh credentials). The
		// provider fills from the repository *base* URL (not this service endpoint) so a path-sensitive
		// lookup matches git's; the `401`'s challenge rides along so a helper receives it as `wwwauth[]`.
		let Some(cred_request) =
			CredentialRequest::from_url(&self.base_url, self.username_hint.clone())
				.map(|request| request.with_wwwauth(response.www_authenticate.clone()))
		else {
			return response.into_body(url);
		};
		let Some(credential) = self.provider.fill(&cred_request).await? else {
			return response.into_body(url);
		};
		let retry = self.send(&request, &auth_headers(&credential)).await?;
		if retry.status == 401 {
			// The filled credential was rejected too — erase it (git's `credential_reject`) and let the
			// server's 401 stand.
			self.note_rejected(&credential).await;
			return retry.into_body(url);
		}
		if retry.is_success() {
			self.note_approved(&credential).await;
			*self.cached.lock().expect("cache not poisoned") = Some(credential);
		}
		retry.into_body(url)
	}
}

/// The `Authorization: Basic …` header for `credential`, as a one-element header list.
fn auth_headers(credential: &Credential) -> Vec<(String, String)> {
	vec![("Authorization".to_owned(), credential.basic_auth_header())]
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
		async fn fill(&self, _request: &CredentialRequest) -> Result<Option<Credential>> {
			Ok(Some(Credential {
				username: self.username.clone(),
				password: self.password.clone(),
			}))
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
		async fn fill(&self, _request: &CredentialRequest) -> Result<Option<Credential>> {
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
		Credential {
			username: username.to_owned(),
			password: password.to_owned(),
		}
		.basic_auth_header()
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

	#[tokio::test]
	async fn a_rejected_url_credential_that_shifts_the_challenge_stops_before_the_provider() {
		let base = "https://example.com/acme/app.git";
		let log = Arc::new(std::sync::Mutex::new(Vec::new()));
		// The URL credential is wrong; its rejection returns a challenge that no longer offers Basic, so
		// the provider (which would panic) must not be consulted and no further Basic credential is sent.
		let transport = AuthTransport::with_userinfo(
			ShiftingChallengeClient { log: log.clone() },
			PanicProvider,
			base.to_owned(),
			Some("alice".to_owned()),
			Some("wrong".to_owned()),
		);

		let result = transport
			.get(&format!("{base}/info/refs?service=git-upload-pack"))
			.await;
		// The server's 401 stands as an error; the provider was never reached (no panic).
		assert!(result.is_err());
		let log = log.lock().unwrap();
		// Exactly two sends: unauthenticated, then the URL credential — and no third (no post-shift Basic).
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
		async fn fill(&self, _request: &CredentialRequest) -> Result<Option<Credential>> {
			Ok(Some(Credential {
				username: self.username.clone(),
				password: self.password.clone(),
			}))
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
		assert_eq!(rejected.lock().unwrap()[0].password, "wrong");
		// Two sends: unauthenticated, then the single filled credential.
		assert_eq!(log.lock().unwrap().len(), 2);
	}
}
