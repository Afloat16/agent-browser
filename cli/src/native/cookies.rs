use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::cdp::client::CdpClient;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CookiePartitionKey {
    pub top_level_site: String,
    pub has_cross_site_ancestor: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    #[serde(default)]
    pub expires: f64,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub session: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition_key: Option<CookiePartitionKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition_key_opaque: Option<bool>,
}

pub fn validate_state_cookies(cookies: &[Cookie]) -> Result<(), String> {
    if cookies
        .iter()
        .any(|cookie| cookie.partition_key_opaque == Some(true))
    {
        return Err("Cannot persist cookies with opaque partition keys".to_string());
    }
    Ok(())
}

pub async fn get_all_cookies(client: &CdpClient, session_id: &str) -> Result<Vec<Cookie>, String> {
    let result = client
        .send_command_no_params("Network.getAllCookies", Some(session_id))
        .await?;

    let cookies: Vec<Cookie> = result
        .get("cookies")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    Ok(cookies)
}

pub async fn get_cookies(
    client: &CdpClient,
    session_id: &str,
    urls: Option<Vec<String>>,
) -> Result<Vec<Cookie>, String> {
    let params = match urls {
        Some(ref u) if !u.is_empty() => json!({ "urls": u }),
        _ => json!({}),
    };

    let result = client
        .send_command("Network.getCookies", Some(params), Some(session_id))
        .await?;

    let cookies: Vec<Cookie> = result
        .get("cookies")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    Ok(cookies)
}

pub async fn set_cookies(
    client: &CdpClient,
    session_id: &str,
    cookies: Vec<Value>,
    current_url: Option<&str>,
) -> Result<(), String> {
    let cookies: Vec<Value> = cookies
        .into_iter()
        .map(|mut c| {
            // Auto-fill url if no domain/path/url provided
            if c.get("url").is_none() && c.get("domain").is_none() && current_url.is_some() {
                c.as_object_mut().map(|m| {
                    m.insert(
                        "url".to_string(),
                        Value::String(current_url.unwrap().to_string()),
                    )
                });
            }
            c
        })
        .collect();

    client
        .send_command(
            "Network.setCookies",
            Some(json!({ "cookies": cookies })),
            Some(session_id),
        )
        .await?;

    Ok(())
}

pub async fn clear_cookies(client: &CdpClient, session_id: &str) -> Result<(), String> {
    client
        .send_command_no_params("Network.clearBrowserCookies", Some(session_id))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitioned_cookie_survives_serialization() {
        for ancestor in [false, true] {
            let original = json!({
                "name": "session", "value": "token", "domain": ".embedded.example", "path": "/",
                "secure": true, "sameSite": "None",
                "partitionKey": { "topLevelSite": "https://top.example", "hasCrossSiteAncestor": ancestor }
            });
            let cookie: Cookie = serde_json::from_value(original.clone()).unwrap();
            let saved = serde_json::to_value(cookie).unwrap();
            assert_eq!(saved["partitionKey"], original["partitionKey"]);
        }
    }

    #[test]
    fn unpartitioned_cookie_does_not_gain_a_partition_key() {
        let cookie: Cookie = serde_json::from_value(json!({
            "name": "session", "value": "token", "domain": ".example.com", "path": "/"
        }))
        .unwrap();
        let saved = serde_json::to_value(cookie).unwrap();
        assert!(saved.get("partitionKey").is_none());
    }

    #[test]
    fn opaque_partition_cannot_be_restored_as_an_unpartitioned_cookie() {
        let cookie: Cookie = serde_json::from_value(json!({
            "name": "session", "value": "token", "domain": ".example.com", "path": "/",
            "partitionKeyOpaque": true
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&cookie).unwrap()["partitionKeyOpaque"],
            true
        );
        assert!(validate_state_cookies(&[cookie])
            .unwrap_err()
            .contains("opaque partition"));
    }

    #[tokio::test]
    async fn partitioned_cookies_survive_cdp_and_state_save_load() {
        use crate::native::state::{load_state, save_state};
        use futures_util::{SinkExt, StreamExt};
        use std::collections::HashSet;
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;

        let cookies = json!([
            { "name": "session", "value": "first", "domain": ".embedded.example", "path": "/", "secure": true,
              "partitionKey": { "topLevelSite": "https://first.example", "hasCrossSiteAncestor": true } },
            { "name": "session", "value": "second", "domain": ".embedded.example", "path": "/", "secure": true,
              "partitionKey": { "topLevelSite": "https://second.example", "hasCrossSiteAncestor": false } }
        ]);
        let expected = cookies.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(message) = ws.next().await {
                let message = message.unwrap();
                if !message.is_text() {
                    continue;
                }
                let request: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                let method = request["method"].as_str().unwrap();
                let result = match method {
                    "Network.getAllCookies" | "Network.getCookies" => json!({ "cookies": cookies }),
                    "Page.getFrameTree" => {
                        json!({ "frameTree": { "frame": { "url": "about:blank" } } })
                    }
                    "Runtime.evaluate" => {
                        json!({ "result": { "type": "object", "value": { "origin": "null" } } })
                    }
                    "Network.setCookies" => json!({}),
                    other => panic!("unexpected CDP method: {other}"),
                };
                ws.send(Message::Text(
                    json!({ "id": request["id"], "result": result }).to_string(),
                ))
                .await
                .unwrap();
                if method == "Network.setCookies" {
                    return request["params"]["cookies"].clone();
                }
            }
            panic!("state load did not send cookies");
        });
        let client = CdpClient::connect(&url).await.unwrap();
        let listed =
            serde_json::to_value(get_cookies(&client, "page", None).await.unwrap()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let saved = save_state(
            &client,
            "page",
            path.to_str(),
            None,
            "test",
            &HashSet::new(),
        )
        .await
        .unwrap();
        load_state(&client, "page", &saved).await.unwrap();
        let restored = server.await.unwrap();
        client.close().await;
        for index in 0..2 {
            assert_eq!(
                listed[index]["partitionKey"],
                expected[index]["partitionKey"]
            );
            assert_eq!(
                restored[index]["partitionKey"],
                expected[index]["partitionKey"]
            );
            assert_eq!(restored[index]["value"], expected[index]["value"]);
        }
    }
}
