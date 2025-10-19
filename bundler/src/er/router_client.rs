use super::{ErClient, ErClientConfig, ErHttpClientInner, ErOutput, ErSession, Result, UserIntent};
use async_trait::async_trait;
use reqwest::Client as HttpClient;
use solana_program::pubkey::Pubkey;

/// HTTP-backed ER client with injectable HTTP handle.
pub struct HttpErClient {
    inner: ErHttpClientInner,
}

impl HttpErClient {
    pub fn new(cfg: ErClientConfig) -> Result<Self> {
        Ok(Self {
            inner: ErHttpClientInner::new(cfg)?,
        })
    }

    pub fn with_http_client(cfg: ErClientConfig, http: HttpClient) -> Result<Self> {
        Ok(Self {
            inner: ErHttpClientInner::with_http_client(cfg, http)?,
        })
    }
}

#[async_trait]
impl ErClient for HttpErClient {
    async fn begin_session(&self, accounts: &[Pubkey]) -> Result<ErSession> {
        self.inner.begin_session(accounts).await
    }

    async fn execute(&self, session: &ErSession, intents: &[UserIntent]) -> Result<ErOutput> {
        self.inner.execute(session, intents).await
    }

    async fn end_session(&self, session: &ErSession) -> Result<()> {
        self.inner.end_session(session).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::er::ErPrivacyMode;
    use serde_json::json;
    use solana_program::instruction::{AccountMeta, Instruction};
    use solana_sdk::signature::Keypair;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::timeout;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_config(base: &str) -> ErClientConfig {
        let url = url::Url::parse(&(base.to_string() + "/")).expect("base url");
        ErClientConfig {
            endpoint_override: None,
            http_timeout: Duration::from_millis(300),
            connect_timeout: Duration::from_millis(200),
            session_ttl: Duration::from_secs(30),
            retries: 1,
            privacy_mode: ErPrivacyMode::Public,
            router_url: url,
            router_api_key: None,
            route_cache_ttl: Duration::from_secs(1),
            circuit_breaker_failures: 2,
            circuit_breaker_cooldown: Duration::from_secs(1),
            payer: Arc::new(Keypair::new()),
            blockhash_cache_ttl: Duration::from_millis(0),
            min_cu_threshold: 0,
            merge_small_intents: false,
            require_router: false,
            wiretap_verify_blockhash: false,
                skip_preflight_on_router: false,
                skip_preflight_on_override: false,
            telemetry: None,
            wiretap: None,
        }
    }

    fn sample_intent() -> UserIntent {
        let program = Pubkey::new_unique();
        let acct = Pubkey::new_unique();
        let ix = Instruction {
            program_id: program,
            accounts: vec![AccountMeta::new(acct, false)],
            data: vec![1, 2, 3],
        };
        UserIntent::new(Pubkey::new_unique(), ix, 0, None)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_client_happy_path() {
        let server = MockServer::start().await;
        let route_body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "fqdn": server.uri(),
                "ttlMs": 1000u64
            }]
        });

        Mock::given(method("POST"))
            .and(path("/getRoutes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(route_body))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(path("/api/v1/er/beginSession"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        Mock::given(path("/api/v1/er/execute"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        Mock::given(path("/api/v1/er/endSession"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let cfg = test_config(&server.uri());
        let accounts = vec![Pubkey::new_unique()];

        let client = HttpErClient::new(cfg.clone()).expect("client");
        let session = timeout(Duration::from_secs(5), client.begin_session(&accounts))
            .await
            .expect("timeout begin")
            .expect("session ok");

        assert_eq!(session.id, "router-session");
        assert_eq!(session.accounts.len(), 1);
        let expected = server.uri();
        assert_eq!(
            session.endpoint.as_str().trim_end_matches('/'),
            expected.as_str().trim_end_matches('/')
        );
        assert_eq!(
            session.route.endpoint.as_str().trim_end_matches('/'),
            expected.as_str().trim_end_matches('/')
        );

        let client_exec = HttpErClient::new(cfg.clone()).expect("client exec");
        let exec_session = session.clone();
        let exec_result = timeout(
            Duration::from_secs(5),
            client_exec.execute(&exec_session, &[sample_intent()]),
        )
        .await
        .expect("timeout execute");
        assert!(exec_result.is_err());

        let client_end = HttpErClient::new(cfg).expect("client end");
        timeout(Duration::from_secs(5), client_end.end_session(&session))
            .await
            .expect("timeout end")
            .expect("end ok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn router_dns_failure() {
        let cfg = test_config("http://nonexistent.invalid");
        let client = HttpErClient::new(cfg).expect("client");
        let result = timeout(
            Duration::from_secs(5),
            client.begin_session(&[Pubkey::new_unique()]),
        )
        .await
        .expect("timeout triggered");
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn router_bad_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getRoutes"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{broken"))
            .mount(&server)
            .await;

        let cfg = test_config(&server.uri());
        let client = HttpErClient::new(cfg).expect("client");
        let result = timeout(
            Duration::from_secs(5),
            client.begin_session(&[Pubkey::new_unique()]),
        )
        .await
        .expect("timeout triggered");
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn router_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/getRoutes"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(600)))
            .mount(&server)
            .await;

        let mut cfg = test_config(&server.uri());
        cfg.http_timeout = Duration::from_millis(300);
        cfg.connect_timeout = Duration::from_millis(200);

        let client = HttpErClient::new(cfg).expect("client");
        let result = timeout(
            Duration::from_secs(5),
            client.begin_session(&[Pubkey::new_unique()]),
        )
        .await
        .expect("timeout triggered");
        assert!(result.is_err());
    }
}
