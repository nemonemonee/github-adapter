use super::*;
use crate::test_support::{Fixture, Reply, authorization};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn context() -> RequestContext {
    RequestContext::new(Duration::from_secs(10)).unwrap()
}

fn device(expires: Duration) -> DeviceAuthorization {
    DeviceAuthorization {
        verification_uri: "https://github.com/login/device".into(),
        user_code: "TEST-CODE".into(),
        expires_in: expires,
        device_code: "fixture-device-secret".into(),
        client_id: "fixture-client".into(),
        interval: Duration::from_millis(2),
        expires_at: Instant::now() + expires,
    }
}

#[test]
fn credentials_trim_only_outer_token_whitespace_and_redact_debug() {
    let credential =
        Credential::new("Personal-User".into(), " \tfixture-oauth\r\n".into()).unwrap();
    assert_eq!(credential.token(), "fixture-oauth");
    assert_eq!(credential.login, "Personal-User");
    assert!(!format!("{credential:?}").contains("fixture-oauth"));
    let same = Credential::new("fixture-oauth".into(), "fixture-oauth".into()).unwrap();
    assert!(!format!("{same:?}").contains("fixture-oauth"));
    for token in [
        "",
        " ",
        "secret inner",
        "secret\ninner",
        "secret\0",
        "secret☃",
    ] {
        let error = Credential::new("user".into(), token.into()).unwrap_err();
        assert_eq!(error.status, 401);
        assert!(!error.message.contains("secret"));
    }
    assert!(Credential::new(" ".into(), "valid-token".into()).is_err());
    assert!(Credential::new("user\nname".into(), "valid-token".into()).is_err());
    assert!(!format!("{:?}", device(Duration::from_secs(10))).contains("fixture-device-secret"));
}

#[test]
fn only_fixed_trusted_copilot_origins_and_unambiguous_prefixes_are_allowed() {
    let auth = AuthClient::new().unwrap();
    for endpoint in [
        "https://api.githubcopilot.com",
        "https://api.individual.githubcopilot.com/",
        "https://api.enterprise.githubcopilot.com/prefix",
    ] {
        assert!(auth.copilot_endpoint(endpoint).is_ok(), "{endpoint}");
    }
    for endpoint in [
        "http://api.githubcopilot.com",
        "https://api.githubcopilot.com.example.invalid",
        "https://githubcopilot.com",
        "https://user@api.githubcopilot.com",
        "https://api.githubcopilot.com:8443",
        "https://api.githubcopilot.com/../path",
        "https://api.githubcopilot.com/%2e%2e/path",
        "https://api.githubcopilot.com/%252e%252e/path",
        "https://api.githubcopilot.com/prefix%2fother",
        "https://api.githubcopilot.com?redirect=other",
        "https://api.githubcopilot.com#fragment",
        "https://api.githubcopilot.com\\other",
    ] {
        assert_eq!(
            auth.copilot_endpoint(endpoint).unwrap_err().status,
            502,
            "{endpoint}"
        );
    }
}

#[tokio::test]
async fn account_identity_is_verified_without_switching_or_copying_cookies() {
    let fixture = Fixture::start(|request, _| {
        assert_eq!(request.path, "/user");
        Reply::json(json!({"login": "Personal-User"}))
            .header("Set-Cookie", "session=should-not-stick")
    })
    .await;
    let http = AuthClient::fixture(fixture.origin.clone()).unwrap();
    let credential = http
        .verify("fixture-oauth", Some("personal-user"), &context())
        .await
        .unwrap();
    assert_eq!(credential.login, "Personal-User");
    let error = http
        .verify("fixture-oauth", Some("other-user"), &context())
        .await
        .unwrap_err();
    assert_eq!(error.status, 403);
    assert!(error.message.contains("No account switch"));
    assert!(!error.message.contains("fixture-oauth"));
    let requests = fixture.requests();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.headers["authorization"],
            format!("token {}", credential.token())
        );
        assert!(!request.headers.contains_key("cookie"));
    }
}

#[tokio::test]
async fn invalid_or_cancelled_public_auth_inputs_fail_before_networking() {
    for token in ["", "inner\nnewline", "snow☃"] {
        assert_eq!(
            verify_account(token, None, &context())
                .await
                .unwrap_err()
                .status,
            401
        );
    }
    assert_eq!(
        verify_account("fixture-oauth", Some(" "), &context())
            .await
            .unwrap_err()
            .status,
        400
    );
    let context = context();
    context.cancellation.cancel();
    assert_eq!(
        verify_account("fixture-oauth", Some("user"), &context)
            .await
            .unwrap_err()
            .status,
        499
    );
    assert_eq!(
        begin_device_login("", &context).await.unwrap_err().status,
        400
    );
}

#[tokio::test]
async fn redirects_errors_and_bad_json_do_not_retry_or_expose_authentication_data() {
    let other = Fixture::start(|_, _| Reply::json(json!({"login": "wrong"}))).await;
    let target = other.origin.to_string();
    let redirect = Fixture::start(move |_, _| {
        Reply::json(json!({"message": "redirect"}))
            .status(302)
            .header("Location", &target)
    })
    .await;
    let http = AuthClient::fixture(redirect.origin.clone()).unwrap();
    assert_eq!(
        http.verify("fixture-oauth", None, &context())
            .await
            .unwrap_err()
            .status,
        502
    );
    assert_eq!(redirect.requests().len(), 1);
    assert!(other.requests().is_empty());
    for status in [401, 403, 429, 500] {
        let fixture = Fixture::start(move |_, _| {
            Reply::raw(
                status,
                "application/json",
                b"fixture-oauth private-body".to_vec(),
            )
        })
        .await;
        let http = AuthClient::fixture(fixture.origin.clone()).unwrap();
        let error = http
            .verify("fixture-oauth", None, &context())
            .await
            .unwrap_err();
        assert_eq!(error.status, status);
        assert!(!error.message.contains("fixture-oauth"));
        assert!(!error.message.contains("private-body"));
        assert_eq!(fixture.requests().len(), 1);
    }
}

#[tokio::test]
async fn device_flow_preserves_client_scope_polling_and_verified_account() {
    let polls = Arc::new(AtomicUsize::new(0));
    let seen = polls.clone();
    let fixture = Fixture::start(move |request, _| match request.path.as_str() {
        "/login/device/code" => Reply::json(json!({
            "verification_uri": "https://github.com/login/device", "user_code": "TEST-CODE",
            "device_code": "fixture-device-secret", "interval": 1, "expires_in": 600,
        })),
        "/login/oauth/access_token" if seen.fetch_add(1, Ordering::SeqCst) == 0 => {
            Reply::json(json!({"error": "authorization_pending"})).status(400)
        }
        "/login/oauth/access_token" => {
            Reply::json(json!({"access_token": "fixture-oauth", "token_type": "Bearer"}))
        }
        "/user" => Reply::json(json!({"login": "Personal-User"})),
        _ => panic!("unexpected authentication endpoint"),
    })
    .await;
    let http = AuthClient::fixture(fixture.origin.clone()).unwrap();
    let mut authorization = http.begin("fixture-client", &context()).await.unwrap();
    assert_eq!(authorization.interval, Duration::from_secs(1));
    assert_eq!(authorization.expires_in, Duration::from_secs(600));
    authorization.interval = Duration::from_millis(2);
    let credential = http
        .poll(
            "fixture-client",
            authorization,
            Some("PERSONAL-USER"),
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(credential.login, "Personal-User");
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    let requests = fixture.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].form()["scope"], "read:user");
    assert_eq!(requests[0].form()["client_id"], "fixture-client");
    for request in &requests[1..3] {
        assert_eq!(request.form()["device_code"], "fixture-device-secret");
        assert_eq!(
            request.form()["grant_type"],
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert!(!request.headers.contains_key("authorization"));
    }
    assert_eq!(requests[3].headers["authorization"], "token fixture-oauth");
}

#[test]
fn slowdown_uses_at_least_five_more_seconds_and_explicit_server_minimum() {
    assert_eq!(
        slowed_interval(Duration::from_secs(1), Some(&json!(3))).unwrap(),
        Duration::from_secs(6)
    );
    assert_eq!(
        slowed_interval(Duration::from_secs(6), Some(&json!(20))).unwrap(),
        Duration::from_secs(20)
    );
    assert_eq!(
        slowed_interval(Duration::from_secs(1), Some(&json!(false))).unwrap(),
        Duration::from_secs(6)
    );
}

#[tokio::test]
async fn expired_declined_slowed_and_cross_client_device_codes_are_not_replayed() {
    for error_code in ["access_denied", "expired_token", "slow_down"] {
        // A mandated interval beyond the code lifetime avoids racing a 100 ms expiry.
        let fixture = Fixture::start(move |_, _| {
            Reply::json(json!({"error": error_code, "interval": 3600})).status(400)
        })
        .await;
        let http = AuthClient::fixture(fixture.origin.clone()).unwrap();
        let error = http
            .poll(
                "fixture-client",
                device(Duration::from_secs(60)),
                None,
                &context(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 401);
        assert_eq!(fixture.requests().len(), 1);
        assert!(!error.message.contains("fixture-device-secret"));
    }
    let fixture = Fixture::start(|_, _| panic!("expired codes must not be sent")).await;
    let http = AuthClient::fixture(fixture.origin.clone()).unwrap();
    assert_eq!(
        http.poll("fixture-client", device(Duration::ZERO), None, &context())
            .await
            .unwrap_err()
            .status,
        401
    );
    assert_eq!(
        http.poll(
            "other-client",
            device(Duration::from_secs(10)),
            None,
            &context()
        )
        .await
        .unwrap_err()
        .status,
        400
    );
    assert!(fixture.requests().is_empty());
}

#[tokio::test]
async fn device_reply_verification_urls_and_token_types_are_validated() {
    for uri in [
        "http://github.com/login/device",
        "https://example.invalid/login/device",
        "https://github.com/login/device?client=other",
        "https://github.com/other",
    ] {
        let fixture = Fixture::start(move |_, _| {
            Reply::json(json!({
                "verification_uri": uri, "user_code": "TEST-CODE", "device_code": "fixture-secret",
                "interval": 1, "expires_in": 600,
            }))
        })
        .await;
        let http = AuthClient::fixture(fixture.origin.clone()).unwrap();
        assert_eq!(
            http.begin("fixture-client", &context())
                .await
                .unwrap_err()
                .status,
            502
        );
    }
    for kind in [Value::Null, json!("basic"), json!(true)] {
        let fixture = Fixture::start(move |_, _| {
            Reply::json(json!({"access_token": "fixture-oauth", "token_type": kind}))
        })
        .await;
        let http = AuthClient::fixture(fixture.origin.clone()).unwrap();
        assert_eq!(
            http.poll(
                "fixture-client",
                device(Duration::from_secs(1)),
                None,
                &context()
            )
            .await
            .unwrap_err()
            .status,
            502
        );
        assert_eq!(fixture.requests().len(), 1);
    }
}

#[tokio::test]
async fn concurrent_requests_share_one_session_and_old_401s_do_not_clear_a_refresh() {
    let exchanges = Arc::new(AtomicUsize::new(0));
    let seen = exchanges.clone();
    let fixture = Fixture::start(move |request, origin| {
        assert_eq!(request.path, "/copilot_internal/v2/token");
        let sequence = seen.fetch_add(1, Ordering::SeqCst);
        Reply::json(authorization(
            origin,
            &format!("fixture-service-{sequence}"),
        ))
    })
    .await;
    let auth = Arc::new(CopilotAuth::new(
        Credential::new("user".into(), "fixture-oauth".into()).unwrap(),
        AuthClient::fixture(fixture.origin.clone()).unwrap(),
    ));
    let context = context();
    let sessions = futures_util::future::join_all((0..16).map(|_| auth.session(&context))).await;
    let first = sessions[0].as_ref().unwrap().clone();
    assert!(
        sessions
            .iter()
            .all(|session| Arc::ptr_eq(session.as_ref().unwrap(), &first))
    );
    assert_eq!(exchanges.load(Ordering::SeqCst), 1);
    auth.invalidate(&first).unwrap();
    let second = auth.session(&context).await.unwrap();
    auth.invalidate(&first).unwrap();
    assert!(Arc::ptr_eq(&second, &auth.session(&context).await.unwrap()));
    assert_eq!(exchanges.load(Ordering::SeqCst), 2);
    assert!(!format!("{second:?}").contains("fixture-service"));
}

#[tokio::test]
async fn failed_session_refresh_and_unsafe_discovery_never_use_a_stale_session() {
    let exchanges = Arc::new(AtomicUsize::new(0));
    let seen = exchanges.clone();
    let fixture = Fixture::start(move |_, origin| {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::json(authorization(origin, "fixture-service"))
        } else {
            Reply::json(json!({"message": "fixture-oauth rejected"})).status(401)
        }
    })
    .await;
    let auth = CopilotAuth::new(
        Credential::new("user".into(), "fixture-oauth".into()).unwrap(),
        AuthClient::fixture(fixture.origin.clone()).unwrap(),
    );
    let first = auth.session(&context()).await.unwrap();
    *auth.current.lock().unwrap() = Some(Arc::new(CopilotSession {
        client: first.client.clone(),
        endpoint: first.endpoint.clone(),
        token: first.token.clone(),
        expires_at: Instant::now(),
        refresh_at: Instant::now(),
    }));
    let error = auth.session(&context()).await.unwrap_err();
    assert_eq!(error.status, 401);
    assert!(!error.message.contains("fixture-oauth"));
    assert!(auth.current.lock().unwrap().is_none());
    assert_eq!(exchanges.load(Ordering::SeqCst), 2);
    let fixture = Fixture::start(|_, origin| {
        let mut reply = authorization(origin, "fixture-service");
        reply["endpoints"]["api"] = json!("https://example.invalid");
        Reply::json(reply)
    })
    .await;
    let auth = CopilotAuth::new(
        Credential::new("user".into(), "fixture-oauth".into()).unwrap(),
        AuthClient::fixture(fixture.origin.clone()).unwrap(),
    );
    assert_eq!(auth.session(&context()).await.unwrap_err().status, 502);
    assert!(auth.current.lock().unwrap().is_none());
    assert_eq!(fixture.requests().len(), 1);
}
