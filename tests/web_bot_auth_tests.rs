use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use pleb::auth::{set_credential, CredentialProvider, CredentialStorage, SecretString};
use pleb::daemon::protocol::{read_bounded_line, write_frame, DaemonRequest, DaemonResponseHeader};
use pleb::daemon::{run_daemon, socket_path, PROTOCOL_VERSION};
use pleb::transport::cef::{CefRenderOptions, CefRenderer};
use pleb::transport::web_bot_auth::{
    ed25519_jwk_thumbprint, execute_signed_get, WebBotAuthSigner, MAX_CACHED_AUTHORITIES,
    MAX_SIGNED_REDIRECT_HOPS, REFRESH_SKEW_SECS, SIGNATURE_TTL_SECS,
};
use pleb::transport::{
    transport_error_kind, RequestHeaderOptions, TransportClient, TransportErrorKind,
    WebBotAuthHeaders,
};
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use url::Url;

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct TempStateDir {
    path: PathBuf,
}

impl TempStateDir {
    fn new() -> Self {
        let pid = std::process::id();
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/tmp/pleb-t-wba-{pid}-{seq}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp state dir");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700 temp state dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempStateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn new(vars: &[(&'static str, Option<&str>)]) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut saved = Vec::with_capacity(vars.len() + 2);
        saved.push(("PLEB_NO_KEYCHAIN", std::env::var("PLEB_NO_KEYCHAIN").ok()));
        saved.push(("PLEB_NO_HOME_CEF", std::env::var("PLEB_NO_HOME_CEF").ok()));
        std::env::set_var("PLEB_NO_KEYCHAIN", "1");
        std::env::set_var("PLEB_NO_HOME_CEF", "1");
        for &(key, val) in vars {
            saved.push((key, std::env::var(key).ok()));
            match val {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        Self { _lock: lock, saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prev) in self.saved.iter().rev() {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

const RFC8037_D: &str = "nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A";
const RFC8037_X: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
const RFC8037_THUMBPRINT: &str = "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k";
const AGENT_URL: &str = "https://web-bot-auth.example.workers.dev";

fn rfc8037_jwk_json() -> String {
    format!(r#"{{"crv":"Ed25519","d":"{RFC8037_D}","kty":"OKP","x":"{RFC8037_X}"}}"#)
}

fn rfc8037_signer() -> WebBotAuthSigner {
    WebBotAuthSigner::from_credentials(
        &SecretString::new(rfc8037_jwk_json()),
        &SecretString::new(AGENT_URL),
    )
    .expect("RFC 8037 signer should initialize")
}

fn verify_wba_headers(authority: &str, headers: &WebBotAuthHeaders, x_b64: &str) -> Result<(), ()> {
    let x_bytes = URL_SAFE_NO_PAD.decode(x_b64).map_err(|_| ())?;
    let sig_params = headers.signature_input.strip_prefix("sig1=").ok_or(())?;
    let sig_b64 = headers
        .signature
        .strip_prefix("sig1=:")
        .and_then(|s| s.strip_suffix(':'))
        .ok_or(())?;
    let sig_bytes = STANDARD.decode(sig_b64).map_err(|_| ())?;
    let base = format!(
        "\"@authority\": {authority}\n\"signature-agent\": {}\n\"@signature-params\": {sig_params}",
        headers.signature_agent
    );
    UnparsedPublicKey::new(&ED25519, &x_bytes)
        .verify(base.as_bytes(), &sig_bytes)
        .map_err(|_| ())
}

#[test]
fn rfc7638_thumbprint_ignores_jwk_kid_and_matches_rfc8037_vector() {
    let x_bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(RFC8037_X)
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(ed25519_jwk_thumbprint(&x_bytes), RFC8037_THUMBPRINT);

    let jwk_with_kid = format!(
        r#"{{
            "kty": "OKP",
            "crv": "Ed25519",
            "x": "{RFC8037_X}",
            "d": "{RFC8037_D}",
            "kid": "a-kid-we-must-ignore"
        }}"#
    );
    let signer = WebBotAuthSigner::from_credentials(
        &SecretString::new(jwk_with_kid),
        &SecretString::new(AGENT_URL),
    )
    .unwrap();
    assert_eq!(signer.key_id(), RFC8037_THUMBPRINT);

    let headers = signer
        .sign_url_at(
            &Url::parse("https://example.com/some/path").unwrap(),
            1_700_000_000,
        )
        .unwrap()
        .expect("public https URL should be signed");
    assert!(headers
        .signature_input
        .contains(&format!("keyid=\"{RFC8037_THUMBPRINT}\"")));
    assert!(!headers.signature_input.contains("a-kid-we-must-ignore"));
}

#[test]
fn rfc9421_headers_verify_with_ring_and_handle_ports_and_ipv6() {
    let signer = rfc8037_signer();

    // Default :443 omitted in @authority.
    let h_default = signer
        .sign_url_at(
            &Url::parse("https://Example.COM:443/docs?a=1").unwrap(),
            1_700_000_000,
        )
        .unwrap()
        .unwrap();
    assert_eq!(h_default.signature_agent, format!("\"{AGENT_URL}\""));
    assert!(h_default
        .signature_input
        .starts_with("sig1=(\"@authority\" \"signature-agent\");"));
    assert!(h_default
        .signature_input
        .contains(";created=1700000000;expires=1700000300;"));
    assert!(h_default
        .signature_input
        .contains(";alg=\"ed25519\";tag=\"web-bot-auth\""));
    verify_wba_headers("example.com", &h_default, RFC8037_X).expect("default port verifies");
    assert!(verify_wba_headers("evil.example", &h_default, RFC8037_X).is_err());

    // Tampered signature-agent fails verification.
    let mut tampered = h_default.clone();
    tampered.signature_agent = "\"https://attacker.example\"".to_string();
    assert!(verify_wba_headers("example.com", &tampered, RFC8037_X).is_err());

    // Non-443 port preserved in @authority and cached separately.
    let h_port = signer
        .sign_url_at(
            &Url::parse("https://example.com:8443/x").unwrap(),
            1_700_000_000,
        )
        .unwrap()
        .unwrap();
    assert_ne!(h_port.signature, h_default.signature);
    verify_wba_headers("example.com:8443", &h_port, RFC8037_X).expect("port authority verifies");
    assert!(verify_wba_headers("example.com", &h_port, RFC8037_X).is_err());

    // IPv6 host bracketed cleanly without double-bracketing.
    let h_v6 = signer
        .sign_url_at(
            &Url::parse("https://[2001:4860:4860::8888]:8443/dns").unwrap(),
            1_700_000_000,
        )
        .unwrap()
        .unwrap();
    verify_wba_headers("[2001:4860:4860::8888]:8443", &h_v6, RFC8037_X)
        .expect("IPv6 authority verifies");

    // Different key fails verification.
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let other_kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let other_x = URL_SAFE_NO_PAD.encode(other_kp.public_key().as_ref());
    assert!(verify_wba_headers("example.com", &h_default, &other_x).is_err());
}

#[test]
fn refuses_http_private_and_loopback_urls_unless_test_loopback_enabled() {
    let signer = rfc8037_signer();
    for ineligible in [
        "http://example.com/",
        "https://localhost/admin",
        "https://127.0.0.1:8443/",
        "https://[::1]:8443/",
        "https://10.0.0.1/",
        "https://172.16.0.1/",
        "https://192.168.1.1/",
        "https://169.254.169.254/latest/meta-data/",
        "https://100.64.0.1/",
        "https://host.local/",
        "https://metadata.internal/",
    ] {
        let url = Url::parse(ineligible).unwrap();
        assert_eq!(
            signer.sign_url(&url).unwrap(),
            None,
            "ineligible URL must not be signed: {ineligible}"
        );
    }

    let test_signer = rfc8037_signer().with_allow_loopback_for_test(true);
    let loopback_https = Url::parse("https://127.0.0.1:8443/ok").unwrap();
    let signed = test_signer
        .sign_url(&loopback_https)
        .unwrap()
        .expect("loopback https allowed when test flag set");
    verify_wba_headers("127.0.0.1:8443", &signed, RFC8037_X).unwrap();

    // Plaintext http and non-loopback private IPs remain refused even with test flag.
    assert!(test_signer
        .sign_url(&Url::parse("http://127.0.0.1:8443/ok").unwrap())
        .unwrap()
        .is_none());
    assert!(test_signer
        .sign_url(&Url::parse("https://10.0.0.1/").unwrap())
        .unwrap()
        .is_none());
}

#[test]
fn authority_cache_reuses_within_ttl_invalidates_on_backward_clock_and_bounds_256_entries() {
    let signer = rfc8037_signer();
    let url = Url::parse("https://cache.example/page").unwrap();
    let base = 1_700_000_000u64;

    let h0 = signer.sign_url_at(&url, base).unwrap().unwrap();
    // Reused right before skew window.
    let h_reuse = signer
        .sign_url_at(&url, base + SIGNATURE_TTL_SECS - REFRESH_SKEW_SECS - 1)
        .unwrap()
        .unwrap();
    assert_eq!(h_reuse, h0);

    // Refreshed at skew boundary.
    let h_refresh = signer
        .sign_url_at(&url, base + SIGNATURE_TTL_SECS - REFRESH_SKEW_SECS)
        .unwrap()
        .unwrap();
    assert_ne!(h_refresh.signature, h0.signature);
    assert!(h_refresh.signature_input.contains(&format!(
        "created={}",
        base + SIGNATURE_TTL_SECS - REFRESH_SKEW_SECS
    )));

    // Clock stepped backwards below cached `created` invalidates cached entry immediately.
    let h_backward = signer.sign_url_at(&url, base + 10).unwrap().unwrap();
    assert_ne!(h_backward.signature, h_refresh.signature);
    assert!(h_backward
        .signature_input
        .contains(&format!("created={}", base + 10)));

    // 256-entry bound with min-seq eviction.
    let bounded_signer = rfc8037_signer();
    let mut initial = Vec::with_capacity(MAX_CACHED_AUTHORITIES);
    for i in 0..MAX_CACHED_AUTHORITIES {
        let u = Url::parse(&format!("https://host-{i}.example/")).unwrap();
        let h = bounded_signer.sign_url_at(&u, base).unwrap().unwrap();
        initial.push(h);
    }

    // Confirm entry 0 and entry 1 are still cached before overflow.
    assert_eq!(
        bounded_signer
            .sign_url_at(&Url::parse("https://host-0.example/").unwrap(), base + 5)
            .unwrap()
            .unwrap(),
        initial[0]
    );
    assert_eq!(
        bounded_signer
            .sign_url_at(&Url::parse("https://host-1.example/").unwrap(), base + 5)
            .unwrap()
            .unwrap(),
        initial[1]
    );

    // Insert 257th authority -> evicts host-0 (lowest seq).
    let _ = bounded_signer
        .sign_url_at(&Url::parse("https://host-256.example/").unwrap(), base + 5)
        .unwrap()
        .unwrap();

    // host-1..255 remain cached while host-0 is evicted and re-signed.
    assert_eq!(
        bounded_signer
            .sign_url_at(&Url::parse("https://host-1.example/").unwrap(), base + 5)
            .unwrap()
            .unwrap(),
        initial[1]
    );
    let re_signed_0 = bounded_signer
        .sign_url_at(&Url::parse("https://host-0.example/").unwrap(), base + 5)
        .unwrap()
        .unwrap();
    assert_ne!(re_signed_0.signature, initial[0].signature);
}

#[tokio::test]
async fn outbound_wba_headers_reject_control_bytes_and_deliver_over_direct_and_daemon_v4() {
    let dir = TempStateDir::new();
    let dir_clone = dir.path().to_path_buf();
    let daemon_handle =
        tokio::spawn(async move { run_daemon(dir_clone, Duration::from_secs(30), false).await });

    // Wait for daemon socket.
    for _ in 0..80 {
        if pleb::daemon::daemon_ping(dir.path()).await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let direct_dir = TempStateDir::new();
    let direct = TransportClient::new_direct(direct_dir.path(), None, Duration::from_secs(5))
        .expect("direct transport");
    let daemon = TransportClient::new_daemon(
        dir.path().to_path_buf(),
        None,
        Duration::from_secs(5),
        Duration::from_secs(30),
        false,
    );

    // Reject \r, \n, \0 in outbound WebBotAuthHeaders.
    for bad in ["sig1=:abc\rdef:", "sig1=:abc\ndef:", "sig1=:abc\0def:"] {
        let opts = RequestHeaderOptions {
            web_bot_auth: Some(WebBotAuthHeaders {
                signature: bad.to_string(),
                signature_input: "sig1=(\"@authority\");created=1".to_string(),
                signature_agent: format!("\"{AGENT_URL}\""),
            }),
            ..Default::default()
        };
        for client in [&direct, &daemon] {
            let err = client
                .get_with_request_options(
                    "http://127.0.0.1:9/bad",
                    Duration::from_secs(2),
                    1024,
                    &opts,
                )
                .await
                .unwrap_err();
            assert_eq!(
                transport_error_kind(&err),
                Some(TransportErrorKind::Network)
            );
        }
    }

    // Real TCP server verifying wire headers and unfollowed 302 redirect capture.
    let next_hits = Arc::new(AtomicUsize::new(0));
    let next_hits_srv = Arc::clone(&next_hits);
    let captured_headers: Arc<Mutex<Vec<HashMap<String, String>>>> =
        Arc::new(Mutex::new(Vec::new()));
    let captured_srv = Arc::clone(&captured_headers);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let srv = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let next_hits = Arc::clone(&next_hits_srv);
            let captured = Arc::clone(&captured_srv);
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let raw = String::from_utf8_lossy(&buf[..n]);
                let mut lines = raw.lines();
                let path = lines
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let mut map = HashMap::new();
                for line in lines {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        map.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                captured.lock().unwrap().push(map);
                let resp = match path {
                    "/signed" => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    "/redir" => "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    "/next" => {
                        next_hits.fetch_add(1, Ordering::SeqCst);
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnext"
                    }
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                };
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });

    let signer = rfc8037_signer();
    let signed = signer
        .sign_url(&Url::parse("https://wire.example/signed").unwrap())
        .unwrap()
        .unwrap();
    let opts = RequestHeaderOptions {
        web_bot_auth: Some(signed.clone()),
        disable_redirects: true,
        ..Default::default()
    };

    for client in [&direct, &daemon] {
        let resp = client
            .get_with_request_options(
                &format!("http://{addr}/signed"),
                Duration::from_secs(5),
                64 * 1024,
                &opts,
            )
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.text(), "ok");

        let redir_resp = client
            .get_with_request_options(
                &format!("http://{addr}/redir"),
                Duration::from_secs(5),
                64 * 1024,
                &opts,
            )
            .await
            .unwrap();
        assert_eq!(redir_resp.status, 302);
        let loc = redir_resp
            .location_header
            .as_deref()
            .expect("302 must preserve Location without following");
        assert!(loc.ends_with("/next"), "unexpected Location: {loc}");
    }

    assert_eq!(
        next_hits.load(Ordering::SeqCst),
        0,
        "redirect target /next must not be fetched when disable_redirects/web_bot_auth is set"
    );

    {
        let recorded = captured_headers.lock().unwrap();
        assert_eq!(recorded.len(), 4);
        for hdrs in recorded.iter() {
            let wire_wba = WebBotAuthHeaders {
                signature: hdrs.get("signature").cloned().expect("Signature header"),
                signature_input: hdrs
                    .get("signature-input")
                    .cloned()
                    .expect("Signature-Input header"),
                signature_agent: hdrs
                    .get("signature-agent")
                    .cloned()
                    .expect("Signature-Agent header"),
            };
            verify_wba_headers("wire.example", &wire_wba, RFC8037_X)
                .expect("wire headers must verify cryptographically");
        }
    }

    srv.abort();
    assert!(pleb::daemon::daemon_stop(dir.path()).await.unwrap());
    let _ = daemon_handle.await;
}

#[derive(Debug)]
struct RecordedDaemonHop {
    url: String,
    disable_redirects: bool,
    web_bot_auth: Option<WebBotAuthHeaders>,
    referer: Option<String>,
}

async fn spawn_mock_v4_daemon(
    state_dir: &Path,
    routes: Vec<(String, u16, Option<String>, Vec<u8>)>,
) -> (
    Arc<Mutex<Vec<RecordedDaemonHop>>>,
    tokio::task::JoinHandle<()>,
) {
    let sock = socket_path(state_dir);
    let listener = UnixListener::bind(&sock).unwrap();
    let hops = Arc::new(Mutex::new(Vec::new()));
    let hops_c = Arc::clone(&hops);

    let handle = tokio::spawn(async move {
        let mut route_map: HashMap<String, (u16, Option<String>, Vec<u8>)> = routes
            .into_iter()
            .map(|(u, st, loc, body)| (u, (st, loc, body)))
            .collect();
        while let Ok((stream, _)) = listener.accept().await {
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let Some(line) = read_bounded_line(&mut reader, 64 * 1024).await.unwrap() else {
                continue;
            };
            let req: DaemonRequest = serde_json::from_str(line.trim()).unwrap();
            if let DaemonRequest::HttpGet {
                url,
                web_bot_auth,
                disable_redirects,
                referer,
                expected_protocol_version,
                ..
            } = req
            {
                assert_eq!(expected_protocol_version, Some(PROTOCOL_VERSION));
                hops_c.lock().unwrap().push(RecordedDaemonHop {
                    url: url.clone(),
                    disable_redirects,
                    web_bot_auth,
                    referer,
                });
                let (status, loc, body) =
                    route_map
                        .remove(&url)
                        .unwrap_or((404, None, b"not found".to_vec()));
                let mut hdr = DaemonResponseHeader::ok_empty();
                hdr.status_code = Some(status);
                hdr.final_url = Some(url);
                hdr.content_type = Some("text/plain".to_string());
                hdr.location_header = loc;
                hdr.body_bytes = body.len();
                write_frame(&mut write_half, &hdr, &body).await.unwrap();
            }
        }
    });

    (hops, handle)
}

#[tokio::test]
async fn execute_signed_get_handles_cross_authority_resigning_downgrade_ssrf_and_hop_limit_over_daemon_v4(
) {
    let signer = rfc8037_signer();

    // 1. Cross-authority re-signing + relative redirect + https->http downgrade stripping.
    {
        let dir = TempStateDir::new();
        let routes = vec![
            (
                "https://alpha.example/start".to_string(),
                302,
                Some("https://beta.example:8443/step2".to_string()),
                Vec::new(),
            ),
            (
                "https://beta.example:8443/step2".to_string(),
                307,
                Some("/relative".to_string()),
                Vec::new(),
            ),
            (
                "https://beta.example:8443/relative".to_string(),
                301,
                Some("http://plain.example/final".to_string()),
                Vec::new(),
            ),
            (
                "http://plain.example/final".to_string(),
                200,
                None,
                b"reached".to_vec(),
            ),
        ];
        let (hops, srv) = spawn_mock_v4_daemon(dir.path(), routes).await;
        let client = TransportClient::new_daemon(
            dir.path().to_path_buf(),
            None,
            Duration::from_secs(5),
            Duration::from_secs(30),
            false,
        );
        let opts = RequestHeaderOptions {
            referer: Some("https://alpha.example/orig".to_string()),
            ..Default::default()
        };
        let resp = execute_signed_get(
            &client,
            "https://alpha.example/start",
            Duration::from_secs(5),
            64 * 1024,
            &opts,
            Some(&signer),
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.text(), "reached");

        let recorded = hops.lock().unwrap();
        assert_eq!(recorded.len(), 4);
        assert!(recorded.iter().all(|h| h.disable_redirects));

        // Hop 0: alpha.example signed.
        let wba0 = recorded[0].web_bot_auth.as_ref().expect("hop 0 signed");
        verify_wba_headers("alpha.example", wba0, RFC8037_X).unwrap();
        assert!(verify_wba_headers("beta.example:8443", wba0, RFC8037_X).is_err());

        // Hop 1: beta.example:8443 re-signed with distinct signature.
        let wba1 = recorded[1].web_bot_auth.as_ref().expect("hop 1 signed");
        verify_wba_headers("beta.example:8443", wba1, RFC8037_X).unwrap();
        assert_ne!(wba1.signature, wba0.signature);

        // Hop 2: same authority beta.example:8443 reuses cached signature.
        let wba2 = recorded[2].web_bot_auth.as_ref().expect("hop 2 signed");
        assert_eq!(wba2, wba1);

        // Hop 3: downgraded to http:// -> Signature* and https Referer stripped!
        assert_eq!(recorded[3].url, "http://plain.example/final");
        assert!(
            recorded[3].web_bot_auth.is_none(),
            "https->http downgrade must strip Signature headers"
        );
        assert!(
            recorded[3].referer.is_none(),
            "https->http downgrade must drop https Referer"
        );
        srv.abort();
    }

    // 2. 3xx SSRF blocking (IMDS + localhost) never sends second hop.
    for blocked in [
        "http://169.254.169.254/latest/meta-data/",
        "https://localhost/internal",
        "http://[::ffff:127.0.0.1]/admin",
    ] {
        let dir = TempStateDir::new();
        let routes = vec![(
            "https://alpha.example/ssrf".to_string(),
            302,
            Some(blocked.to_string()),
            Vec::new(),
        )];
        let (hops, srv) = spawn_mock_v4_daemon(dir.path(), routes).await;
        let client = TransportClient::new_daemon(
            dir.path().to_path_buf(),
            None,
            Duration::from_secs(5),
            Duration::from_secs(30),
            false,
        );
        let err = execute_signed_get(
            &client,
            "https://alpha.example/ssrf",
            Duration::from_secs(5),
            64 * 1024,
            &RequestHeaderOptions::default(),
            Some(&signer),
        )
        .await
        .unwrap_err();
        assert_eq!(
            transport_error_kind(&err),
            Some(TransportErrorKind::RedirectBlocked)
        );
        assert_eq!(hops.lock().unwrap().len(), 1);
        srv.abort();
    }

    // 3. Exactly 10 redirects succeed; 11 redirects (> MAX_SIGNED_REDIRECT_HOPS) fail with LoadError.
    {
        let dir_ok = TempStateDir::new();
        let mut routes_ok = Vec::new();
        for i in 0..MAX_SIGNED_REDIRECT_HOPS {
            routes_ok.push((
                format!("https://hop-{i}.example/p"),
                302,
                Some(format!("https://hop-{}.example/p", i + 1)),
                Vec::new(),
            ));
        }
        routes_ok.push((
            format!("https://hop-{MAX_SIGNED_REDIRECT_HOPS}.example/p"),
            200,
            None,
            b"ten-hops-ok".to_vec(),
        ));
        let (hops_ok, srv_ok) = spawn_mock_v4_daemon(dir_ok.path(), routes_ok).await;
        let client_ok = TransportClient::new_daemon(
            dir_ok.path().to_path_buf(),
            None,
            Duration::from_secs(5),
            Duration::from_secs(30),
            false,
        );
        let resp = execute_signed_get(
            &client_ok,
            "https://hop-0.example/p",
            Duration::from_secs(5),
            64 * 1024,
            &RequestHeaderOptions::default(),
            Some(&signer),
        )
        .await
        .unwrap();
        assert_eq!(resp.text(), "ten-hops-ok");
        assert_eq!(hops_ok.lock().unwrap().len(), MAX_SIGNED_REDIRECT_HOPS + 1);
        srv_ok.abort();

        let dir_too_many = TempStateDir::new();
        let mut routes_err = Vec::new();
        for i in 0..=MAX_SIGNED_REDIRECT_HOPS {
            routes_err.push((
                format!("https://hop-{i}.example/p"),
                302,
                Some(format!("https://hop-{}.example/p", i + 1)),
                Vec::new(),
            ));
        }
        let (hops_err, srv_err) = spawn_mock_v4_daemon(dir_too_many.path(), routes_err).await;
        let client_err = TransportClient::new_daemon(
            dir_too_many.path().to_path_buf(),
            None,
            Duration::from_secs(5),
            Duration::from_secs(30),
            false,
        );
        let err = execute_signed_get(
            &client_err,
            "https://hop-0.example/p",
            Duration::from_secs(5),
            64 * 1024,
            &RequestHeaderOptions::default(),
            Some(&signer),
        )
        .await
        .unwrap_err();
        assert_eq!(
            transport_error_kind(&err),
            Some(TransportErrorKind::LoadError)
        );
        assert_eq!(hops_err.lock().unwrap().len(), MAX_SIGNED_REDIRECT_HOPS + 1);
        srv_err.abort();
    }
}

#[tokio::test]
async fn execute_signed_get_with_direct_client_blocks_ssrf_redirects_and_strips_http() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let saw_signature = Arc::new(AtomicUsize::new(0));
    let saw_sig_srv = Arc::clone(&saw_signature);

    let srv = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let saw_sig = Arc::clone(&saw_sig_srv);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let raw = String::from_utf8_lossy(&buf[..n]);
                let mut lines = raw.lines();
                let path = lines
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                for line in lines {
                    if line.to_ascii_lowercase().starts_with("signature:") {
                        saw_sig.fetch_add(1, Ordering::SeqCst);
                    }
                }
                let resp = match path {
                    "/start" => "HTTP/1.1 302 Found\r\nLocation: /done\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    "/done" => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone",
                    "/imds" => "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                };
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });

    let dir = TempStateDir::new();
    let direct = TransportClient::new_direct(dir.path(), None, Duration::from_secs(5)).unwrap();
    let signer = rfc8037_signer();

    let resp = execute_signed_get(
        &direct,
        &format!("http://{addr}/start"),
        Duration::from_secs(5),
        64 * 1024,
        &RequestHeaderOptions::default(),
        Some(&signer),
    )
    .await
    .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.text(), "done");
    assert_eq!(
        saw_signature.load(Ordering::SeqCst),
        0,
        "http:// must remain unsigned in execute_signed_get"
    );

    let err = execute_signed_get(
        &direct,
        &format!("http://{addr}/imds"),
        Duration::from_secs(5),
        64 * 1024,
        &RequestHeaderOptions::default(),
        Some(&signer),
    )
    .await
    .unwrap_err();
    assert_eq!(
        transport_error_kind(&err),
        Some(TransportErrorKind::RedirectBlocked)
    );
    srv.abort();
}

#[tokio::test]
async fn daemon_and_cef_child_spawns_scrub_all_credential_envs_and_cdp_url() {
    let mut env_pairs: Vec<(&'static str, Option<&str>)> = CredentialProvider::ALL
        .iter()
        .map(|p| (p.env_var(), Some("secret-should-be-scrubbed")))
        .collect();
    env_pairs.push((
        "PLEB_CDP_URL",
        Some("wss://cdp.example/devtools/browser?token=secret"),
    ));
    let _env = EnvGuard::new(&env_pairs);

    // 1. Test spawn_detached_daemon env scrubbing.
    let daemon_dir = TempStateDir::new();
    let daemon_env_dump = daemon_dir.path().join("daemon-env.json");
    let mock_daemon = daemon_dir.path().join("mock-daemon.py");
    let daemon_script = format!(
        r#"#!/usr/bin/env python3
import json, os, socket, sys

args = sys.argv[1:]
state_dir = args[args.index("--state-dir") + 1]
with open({dump_path:?}, "w") as f:
    json.dump(dict(os.environ), f)

sock_path = os.path.join(state_dir, "daemon.sock")
if os.path.exists(sock_path):
    os.unlink(sock_path)
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(4)
conn, _ = srv.accept()
_ = conn.recv(65536)
hdr = {{
    "ok": True,
    "protocol_version": {proto},
    "build_id": {build_id:?},
    "status_code": 200,
    "final_url": "http://127.0.0.1/mock",
    "body_bytes": 2,
}}
conn.sendall((json.dumps(hdr) + "\n").encode() + b"ok")
conn.close()
srv.close()
if os.path.exists(sock_path):
    os.unlink(sock_path)
"#,
        dump_path = daemon_env_dump,
        proto = PROTOCOL_VERSION,
        build_id = pleb::daemon::BUILD_ID,
    );
    std::fs::write(&mock_daemon, daemon_script).unwrap();
    std::fs::set_permissions(&mock_daemon, std::fs::Permissions::from_mode(0o755)).unwrap();

    let daemon_client = TransportClient::new_daemon(
        daemon_dir.path().to_path_buf(),
        None,
        Duration::from_secs(5),
        Duration::from_secs(10),
        false,
    )
    .with_daemon_executable(Some(mock_daemon));
    let daemon_resp = daemon_client
        .get("http://127.0.0.1/mock")
        .await
        .expect("mock daemon should handle request");
    assert_eq!(daemon_resp.text(), "ok");

    let daemon_env: HashMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(&daemon_env_dump).unwrap()).unwrap();
    for provider in CredentialProvider::ALL {
        assert!(
            !daemon_env.contains_key(provider.env_var()),
            "daemon child inherited {}",
            provider.env_var()
        );
    }
    assert!(
        !daemon_env.contains_key("PLEB_CDP_URL"),
        "daemon child inherited PLEB_CDP_URL"
    );

    // 2. Test CefRenderer::spawn_and_await_ready env scrubbing.
    let cef_dir = TempStateDir::new();
    let renderer_bin = cef_dir.path().join("mock-pleb-renderer");
    let renderer_script = r#"#!/usr/bin/env python3
# PLEB_RENDERER_PROTOCOL=v5-meta
import json, os, sys

sys.stdout.write(json.dumps({"ready": True, "protocol": "v5-meta", "error": None}) + "\n")
sys.stdout.flush()

for raw in sys.stdin:
    line = raw.strip()
    if not line:
        break
    req = json.loads(line)
    payload = json.dumps(sorted(os.environ.keys())).encode("utf-8")
    hdr = {"status_code": 200, "final_url": req["url"], "html_bytes": len(payload), "error": None}
    sys.stdout.write(json.dumps(hdr) + "\n")
    sys.stdout.flush()
    sys.stdout.buffer.write(payload)
    sys.stdout.buffer.flush()
"#;
    std::fs::write(&renderer_bin, renderer_script).unwrap();
    std::fs::set_permissions(&renderer_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let cef = CefRenderer::new_direct(cef_dir.path().to_path_buf(), None, false)
        .with_renderer_executable(Some(renderer_bin));
    let cef_res = cef
        .render(
            "https://mock.example/",
            &CefRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .expect("mock CEF renderer should run");
    let cef_env_keys: Vec<String> = serde_json::from_str(&cef_res.html).unwrap();
    for provider in CredentialProvider::ALL {
        assert!(
            !cef_env_keys.iter().any(|k| k == provider.env_var()),
            "CEF child inherited {}",
            provider.env_var()
        );
    }
    assert!(
        !cef_env_keys.iter().any(|k| k == "PLEB_CDP_URL"),
        "CEF child inherited PLEB_CDP_URL"
    );

    // Ensure WebBotAuthSigner::from_state_dir works with valid stored creds.
    let wba_dir = TempStateDir::new();
    set_credential(
        wba_dir.path(),
        CredentialProvider::WebBotAuthJwk,
        &rfc8037_jwk_json(),
        CredentialStorage::File,
    )
    .unwrap();
    set_credential(
        wba_dir.path(),
        CredentialProvider::WebBotAuthAgent,
        AGENT_URL,
        CredentialStorage::File,
    )
    .unwrap();
    // Clear env vars before reading from state_dir.
    drop(_env);
    let _clean = EnvGuard::new(&[
        ("PLEB_WEB_BOT_AUTH_JWK", None),
        ("PLEB_WEB_BOT_AUTH_AGENT", None),
    ]);
    let loaded = WebBotAuthSigner::from_state_dir(wba_dir.path())
        .unwrap()
        .expect("signer should load from state_dir");
    assert_eq!(loaded.key_id(), RFC8037_THUMBPRINT);
    assert_eq!(loaded.signature_agent(), AGENT_URL);
}

#[tokio::test]
async fn adversarial_wba_incomplete_state_header_injection_and_multihop_bounce() {
    // 1. Debug representation must never leak key material.
    let signer = rfc8037_signer();
    let dbg = format!("{signer:?}");
    assert!(!dbg.contains(RFC8037_D), "Debug leaked private d: {dbg}");

    // 2. Incomplete state_dir configurations fail loudly with actionable commands; empty returns None.
    {
        let _clean = EnvGuard::new(&[
            ("PLEB_WEB_BOT_AUTH_JWK", None),
            ("PLEB_WEB_BOT_AUTH_AGENT", None),
        ]);
        let empty_dir = TempStateDir::new();
        assert!(WebBotAuthSigner::from_state_dir(empty_dir.path())
            .unwrap()
            .is_none());

        let jwk_only = TempStateDir::new();
        set_credential(
            jwk_only.path(),
            CredentialProvider::WebBotAuthJwk,
            &rfc8037_jwk_json(),
            CredentialStorage::File,
        )
        .unwrap();
        let err_missing_agent = WebBotAuthSigner::from_state_dir(jwk_only.path())
            .unwrap_err()
            .to_string();
        assert!(
            err_missing_agent.contains("web-bot-auth-agent"),
            "unexpected incomplete error: {err_missing_agent}"
        );

        let agent_only = TempStateDir::new();
        set_credential(
            agent_only.path(),
            CredentialProvider::WebBotAuthAgent,
            AGENT_URL,
            CredentialStorage::File,
        )
        .unwrap();
        let err_missing_jwk = WebBotAuthSigner::from_state_dir(agent_only.path())
            .unwrap_err()
            .to_string();
        assert!(
            err_missing_jwk.contains("web-bot-auth-jwk"),
            "unexpected incomplete error: {err_missing_jwk}"
        );
    }

    // 3. Reject control/non-ASCII bytes in signature_input and signature_agent too.
    let dir = TempStateDir::new();
    let direct = TransportClient::new_direct(dir.path(), None, Duration::from_secs(5)).unwrap();
    for bad_opts in [
        WebBotAuthHeaders {
            signature: "sig1=:dGVzdA==:".to_string(),
            signature_input: "sig1=(\"@authority\");created=1\r\nX-Inject: 1".to_string(),
            signature_agent: format!("\"{AGENT_URL}\""),
        },
        WebBotAuthHeaders {
            signature: "sig1=:dGVzdA==:".to_string(),
            signature_input: "sig1=(\"@authority\");created=1".to_string(),
            signature_agent: format!("\"{AGENT_URL}\x1b[31m\""),
        },
        WebBotAuthHeaders {
            signature: "sig1=:dGVzdA==:".to_string(),
            signature_input: "sig1=(\"@authority\");created=1".to_string(),
            signature_agent: format!("\"{AGENT_URL}/é\""),
        },
    ] {
        let err = direct
            .get_with_request_options(
                "http://127.0.0.1:9/bad",
                Duration::from_secs(2),
                1024,
                &RequestHeaderOptions {
                    web_bot_auth: Some(bad_opts),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            transport_error_kind(&err),
            Some(TransportErrorKind::Network)
        );
    }

    // 4. Protocol-relative //host redirect + https -> http -> https bounce + empty vs malformed Location.
    let mock_dir = TempStateDir::new();
    let routes = vec![
        (
            "https://alpha.example/start".to_string(),
            302,
            Some("//cdn.example:8443/bounce".to_string()),
            Vec::new(),
        ),
        (
            "https://cdn.example:8443/bounce".to_string(),
            302,
            Some("http://plain.example/mid".to_string()),
            Vec::new(),
        ),
        (
            "http://plain.example/mid".to_string(),
            307,
            Some("https://gamma.example/done".to_string()),
            Vec::new(),
        ),
        (
            "https://gamma.example/done".to_string(),
            200,
            None,
            b"bounced-ok".to_vec(),
        ),
        (
            "https://alpha.example/empty-loc".to_string(),
            302,
            Some("   ".to_string()),
            b"stay".to_vec(),
        ),
        (
            "https://alpha.example/bad-loc".to_string(),
            302,
            Some("http://[::1".to_string()),
            Vec::new(),
        ),
    ];
    let (hops, srv) = spawn_mock_v4_daemon(mock_dir.path(), routes).await;
    let daemon = TransportClient::new_daemon(
        mock_dir.path().to_path_buf(),
        None,
        Duration::from_secs(5),
        Duration::from_secs(30),
        false,
    );

    let bounce_resp = execute_signed_get(
        &daemon,
        "https://alpha.example/start",
        Duration::from_secs(5),
        64 * 1024,
        &RequestHeaderOptions {
            referer: Some("https://alpha.example/ref".to_string()),
            ..Default::default()
        },
        Some(&signer),
    )
    .await
    .unwrap();
    assert_eq!(bounce_resp.status, 200);
    assert_eq!(bounce_resp.text(), "bounced-ok");

    let empty_loc_resp = execute_signed_get(
        &daemon,
        "https://alpha.example/empty-loc",
        Duration::from_secs(5),
        64 * 1024,
        &RequestHeaderOptions::default(),
        Some(&signer),
    )
    .await
    .unwrap();
    assert_eq!(empty_loc_resp.status, 302);
    assert_eq!(empty_loc_resp.text(), "stay");

    let bad_loc_err = execute_signed_get(
        &daemon,
        "https://alpha.example/bad-loc",
        Duration::from_secs(5),
        64 * 1024,
        &RequestHeaderOptions::default(),
        Some(&signer),
    )
    .await
    .unwrap_err();
    assert_eq!(
        transport_error_kind(&bad_loc_err),
        Some(TransportErrorKind::RedirectBlocked)
    );

    let recorded = hops.lock().unwrap();
    assert_eq!(recorded.len(), 6);
    verify_wba_headers(
        "cdn.example:8443",
        recorded[1].web_bot_auth.as_ref().unwrap(),
        RFC8037_X,
    )
    .unwrap();
    assert!(
        recorded[2].web_bot_auth.is_none() && recorded[2].referer.is_none(),
        "http:// hop must strip WBA headers and https Referer"
    );
    verify_wba_headers(
        "gamma.example",
        recorded[3].web_bot_auth.as_ref().unwrap(),
        RFC8037_X,
    )
    .unwrap();
    srv.abort();
}
