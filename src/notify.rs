//! Apprise notifications.

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use serde_json::json;

#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()>;
}

/// Posts to an Apprise API server's `/notify` endpoint (`APPRISE_URL`).
pub struct HttpApprise {
    http: reqwest::Client,
    url: String,
}

impl HttpApprise {
    pub fn new(url: String) -> Self {
        Self {
            http: crate::http::client(std::time::Duration::from_secs(15)),
            url,
        }
    }
}

#[async_trait]
impl Notifier for HttpApprise {
    async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .post(&self.url)
            .json(&json!({ "title": title, "body": body }))
            .send()
            .await
            .context("apprise request failed")?;
        if !resp.status().is_success() {
            return Err(anyhow!("apprise returned {}", resp.status()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn posts_title_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/notify"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let n = HttpApprise::new(format!("{}/notify", server.uri()));
        n.notify("Title", "Body").await.unwrap();
    }
}
