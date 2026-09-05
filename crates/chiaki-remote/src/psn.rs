// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// PSN-Remote-Play-Endpunkte — gebündelt aus lib/src/remote/holepunch.c (chiaki-ng).
//
// Dieses Modul ist KEIN 1:1-Port einer C-Datei, sondern die Zusammenfassung
// aller HTTP-Aufrufe (im C: libcurl) und der URL-/Payload-/Header-Definitionen
// aus holepunch.c. Die String-Templates werden exakt übernommen — das C baut
// die Requests bewusst per snprintf, weil die offizielle App kaputtes JSON
// sendet, das wir emulieren (siehe Kommentare im C-Quelltext).
//
// HTTP: ureq (blocking, rustls) statt curl. Verbindungswiederverwendung über
// einen gemeinsamen `ureq::Agent` (C: CURLOPT_SHARE). Explizite "Host:"-Header
// entfallen — ureq leitet sie aus der URL ab (im C identisch zum URL-Host).

use std::time::Duration;

use chiaki_core::error::{ChiakiError, ChiakiResult};

use crate::holepunch::{ConsoleType, DeviceInfo};

// Endpoints we're using (holepunch.c)
pub const DEVICE_LIST_URL_FMT: &str = "https://web.np.playstation.com/api/cloudAssistedNavigation/v2/users/me/clients?platform={platform}&includeFields=device&limit=10&offset=0";
pub const WS_FQDN_API_URL: &str = "https://mobile-pushcl.np.communication.playstation.net/np/serveraddr?version=2.1&fields=keepAliveStatus&keepAliveStatusType=3";
pub const SESSION_CREATE_URL: &str = "https://web.np.playstation.com/api/sessionManager/v1/remotePlaySessions";
pub const SESSION_VIEW_URL: &str = "https://web.np.playstation.com/api/sessionManager/v1/remotePlaySessions?view=v1.0";
pub const USER_PROFILE_URL: &str = "https://asm.np.community.playstation.net/asm/v1/apps/me/baseUrls/userProfile";
pub const WAKEUP_URL_FMT: &str = "{base}/v1/users/{online_id}/remoteConsole/wakeUp?platform=PS4";
pub const SESSION_COMMAND_URL: &str = "https://web.np.playstation.com/api/cloudAssistedNavigation/v2/users/me/commands";
pub const SESSION_MESSAGE_URL_FMT: &str = "https://web.np.playstation.com/api/sessionManager/v1/remotePlaySessions/{session_id}/sessionMessage";
pub const DELETE_MESSAGE_URL_FMT: &str = "https://web.np.playstation.com/api/sessionManager/v1/remotePlaySessions/{session_id}/members/me";

/// Liste der online-STUN-Server (holepunch.c get_stun_servers()).
pub const STUN_HOSTS_URL: &str =
    "https://raw.githubusercontent.com/pradt2/always-online-stun/master/valid_hosts.txt";
pub const STUN_HOSTS_URL_IPV6: &str =
    "https://raw.githubusercontent.com/pradt2/always-online-stun/master/valid_ipv6s.txt";

/// UA-Token, den die offizielle Remote-Play-App sendet.
pub const USER_AGENT_RPNET: &str = "RpNetHttpUtilImpl";

const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const OAUTH_HEADER_FMT: &str = "Bearer {token}";

// ---------------------------------------------------------------------------
// Payload-Builder (1:1 die snprintf-Templates aus holepunch.c)
// ---------------------------------------------------------------------------

/// Port von `session_create_json_fmt`.
pub fn session_create_json(pushctx_id: &str) -> String {
    // "{\"remotePlaySessions\":["
    //     "{\"members\":["
    //         "{\"accountId\":\"me\","
    //          "\"deviceUniqueId\":\"me\","
    //          "\"platform\":\"me\","
    //          "\"pushContexts\":["
    //             "{\"pushContextId\":\"%s\"}]}]}]}"
    format!(
        concat!(
            r#"{{"remotePlaySessions":["#,
            r#"{{"members":["#,
            r#"{{"accountId":"me","#,
            r#""deviceUniqueId":"me","#,
            r#""platform":"me","#,
            r#""pushContexts":["#,
            r#"{{"pushContextId":"{}"}}]}}]}}]}}"#
        ),
        pushctx_id
    )
}

/// Port von `session_start_payload_fmt` (JSON-escape'd, da in einem
/// JSON-String eingebettet).
pub fn session_start_payload(
    account_id: i64,
    session_id: &str,
    data1_base64: &str,
    data2_base64: &str,
) -> String {
    // "{\\\"accountId\\\":%lld,"
    //  "\\\"roomId\\\":0,"
    //  "\\\"sessionId\\\":\\\"%s\\\","
    //  "\\\"clientType\\\":\\\"Windows\\\","
    //  "\\\"data1\\\":\\\"%s\\\","
    //  "\\\"data2\\\":\\\"%s\\\"}"
    format!(
        r#"{{\"accountId\":{account_id},\"roomId\":0,\"sessionId\":\"{session_id}\",\"clientType\":\"Windows\",\"data1\":\"{data1_base64}\",\"data2\":\"{data2_base64}\"}}"#
    )
}

/// Port von `session_start_envelope_fmt`.
pub fn session_start_envelope(device_uid_hex: &str, initial_params: &str, platform: &str) -> String {
    // "{\"commandDetail\":"
    //     "{\"commandType\":\"remotePlay\","
    //      "\"duid\":\"%s\","
    //      "\"messageDestination\":\"SQS\","
    //      "\"parameters\":{\"initialParams\":\"%s\"},"
    //      "\"platform\":\"%s\"}}"
    format!(
        concat!(
            r#"{{"commandDetail":"#,
            r#"{{"commandType":"remotePlay","#,
            r#""duid":"{}","#,
            r#""messageDestination":"SQS","#,
            r#""parameters":{{"initialParams":"{}"}},"#,
            r#""platform":"{}"}}}}"#
        ),
        device_uid_hex, initial_params, platform
    )
}

/// Port von `session_wakeup_envelope_fmt` (PS4-Wakeup).
pub fn session_wakeup_envelope(data1_base64: &str, data2_base64: &str, session_id: &str) -> String {
    // "{\"data\":"
    //     "{\"clientType\":\"Windows\","
    //      "\"data1\":\"%s\","
    //      "\"data2\":\"%s\","
    //      "\"roomId\": 0,"
    //      "\"protocolVer\":\"10.0\","
    //      "\"sessionId\":\"%s\"},"
    //      "\"dataTypeSuffix\":\"remotePlay\"}"
    format!(
        concat!(
            r#"{{"data":"#,
            r#"{{"clientType":"Windows","#,
            r#""data1":"{}","#,
            r#""data2":"{}","#,
            r#""roomId": 0,"#,
            r#""protocolVer":"10.0","#,
            r#""sessionId":"{}"}},"#,
            r#""dataTypeSuffix":"remotePlay"}}"#
        ),
        data1_base64, data2_base64, session_id
    )
}

/// Port von `session_message_envelope_fmt`.
pub fn session_message_envelope(
    payload_body: &str,
    account_id: i64,
    device_uid_hex: &str,
    platform: &str,
) -> String {
    // "{\"channel\":\"remote_play:1\","
    //  "\"payload\":\"ver=1.0, type=text, body=%s\","
    //  "\"to\":["
    //    "{\"accountId\":\"%lld\","
    //     "\"deviceUniqueId\":\"%s\","
    //     "\"platform\":\"%s\"}]}"
    format!(
        concat!(
            r#"{{"channel":"remote_play:1","#,
            r#""payload":"ver=1.0, type=text, body={}","#,
            r#""to":["#,
            r#"{{"accountId":"{}","#,
            r#""deviceUniqueId":"{}","#,
            r#""platform":"{}"}}]}}"#
        ),
        payload_body, account_id, device_uid_hex, platform
    )
}

/// Port von `session_message_fmt`.
pub fn session_message_json(action: &str, req_id: u16, error: u16, conn_request: &str) -> String {
    // "{\\\"action\\\":\\\"%s\\\","
    //  "\\\"reqId\\\":%d,"
    //  "\\\"error\\\":%d,"
    //  "\\\"connRequest\\\":%s}"
    format!(
        r#"{{\"action\":\"{action}\",\"reqId\":{req_id},\"error\":{error},\"connRequest\":{conn_request}}}"#
    )
}

/// Port von `session_connrequest_fmt`.
///
/// NOTE: `local_peer_addr` muss ein leerer String sein, wenn die lokale
/// Peer-Adresse nicht mitgeschickt wird — das ergibt kaputtes JSON, aber die
/// offizielle App macht es genauso (¯\_(ツ)_/¯).
#[allow(clippy::too_many_arguments)]
pub fn session_connrequest_json(
    sid: u16,
    peer_sid: u16,
    skey_b64: &str,
    nat_type: u8,
    candidates_json: &str,
    local_peer_addr_json: &str,
    local_hashed_id_b64: &str,
) -> String {
    // "{\\\"sid\\\":%d,"
    //  "\\\"peerSid\\\":%d,"
    //  "\\\"skey\\\":\\\"%s\\\","
    //  "\\\"natType\\\":%d,"
    //  "\\\"candidate\\\":%s,"
    //  "\\\"defaultRouteMacAddr\\\":\\\"%s\\\","
    //  "\\\"localPeerAddr\\\":%s,"
    //  "\\\"localHashedId\\\":\\\"%s\\\"}"
    format!(
        r#"{{\"sid\":{sid},\"peerSid\":{peer_sid},\"skey\":\"{skey_b64}\",\"natType\":{nat_type},\"candidate\":{candidates_json},\"defaultRouteMacAddr\":\"\",\"localPeerAddr\":{local_peer_addr_json},\"localHashedId\":\"{local_hashed_id_b64}\"}}"#
    )
}

/// Port von `session_connrequest_candidate_fmt`.
pub fn session_connrequest_candidate_json(
    candidate_type: &str,
    addr: &str,
    mapped_addr: &str,
    port: u16,
    mapped_port: u16,
) -> String {
    // "{\\\"type\\\":\\\"%s\\\","
    //  "\\\"addr\\\":\\\"%s\\\","
    //  "\\\"mappedAddr\\\":\\\"%s\\\","
    //  "\\\"port\\\":%d,"
    //  "\\\"mappedPort\\\":%d}"
    format!(
        r#"{{\"type\":\"{candidate_type}\",\"addr\":\"{addr}\",\"mappedAddr\":\"{mapped_addr}\",\"port\":{port},\"mappedPort\":{mapped_port}}}"#
    )
}

/// Port von `session_localpeeraddr_fmt`.
pub fn session_localpeeraddr_json(account_id: i64, platform: &str) -> String {
    // "{\\\"accountId\\\":\\\"%lld\\\","
    //  "\\\"platform\\\":\\\"%s\\\"}"
    format!(
        r#"{{\"accountId\":\"{account_id}\",\"platform\":\"{platform}\"}}"#
    )
}

/// `action_str`-Mapping aus session_message_serialize().
pub fn action_str(action: u8) -> &'static str {
    match action {
        crate::holepunch::SESSION_MESSAGE_ACTION_OFFER => "OFFER",
        crate::holepunch::SESSION_MESSAGE_ACTION_ACCEPT => "ACCEPT",
        crate::holepunch::SESSION_MESSAGE_ACTION_TERMINATE => "TERMINATE",
        crate::holepunch::SESSION_MESSAGE_ACTION_RESULT => "RESULT",
        _ => "UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// Hex/Bytes-Helfer (holepunch.c hex_to_bytes/bytes_to_hex)
// ---------------------------------------------------------------------------

/// Port von `hex_to_bytes()`.
pub fn hex_to_bytes(hex_str: &str, bytes: &mut [u8]) -> ChiakiResult<()> {
    let max_len = bytes.len();
    let hex = hex_str.as_bytes();
    let mut len = hex.len();
    if len > max_len * 2 {
        len = max_len * 2;
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut i = 0;
    while i < len {
        let hi = nibble(hex[i]).ok_or(ChiakiError::InvalidData)?;
        // sscanf("%2hhx") liest auch eine einzelne letzte Ziffer
        let lo = if i + 1 < len {
            nibble(hex[i + 1]).ok_or(ChiakiError::InvalidData)?
        } else {
            0
        };
        bytes[i / 2] = if i + 1 < len {
            (hi << 4) | lo
        } else {
            hi
        };
        i += 2;
    }
    Ok(())
}

/// Port von `bytes_to_hex()`.
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

// ---------------------------------------------------------------------------
// HTTP-Client
// ---------------------------------------------------------------------------

/// Bündelt alle PSN-HTTP-Aufrufe aus holepunch.c (curl → ureq).
pub struct PsnClient {
    agent: ureq::Agent,
    oauth_token: String,
}

impl PsnClient {
    /// `make_oauth2_header` + curl_share_init-Ersatz: Ein Agent pro Session.
    pub fn new(oauth_token: &str) -> PsnClient {
        PsnClient {
            agent: ureq::AgentBuilder::new().build(),
            oauth_token: oauth_token.to_owned(),
        }
    }

    /// Port von `make_oauth2_header()`: ("Authorization", "Bearer <token>").
    pub fn oauth_header(&self) -> (&'static str, String) {
        ("Authorization", OAUTH_HEADER_FMT.replace("{token}", &self.oauth_token))
    }

    /// Der rohe OAuth2-Token (z. B. für den WebSocket-Handshake).
    pub fn oauth_token(&self) -> &str {
        &self.oauth_token
    }

    /// Der gemeinsame HTTP-Agent (C: curl_share) — z. B. für den
    /// UPnP-SOAP-Pfad.
    pub fn agent(&self) -> ureq::Agent {
        self.agent.clone()
    }

    /// Port von `make_session_id_header()`:
    /// Header-Zeile "X-PSN-SESSION-MANAGER-SESSION-IDS: <id>" (als Tupel).
    pub fn session_id_header(session_id: &str) -> (&'static str, String) {
        ("X-PSN-SESSION-MANAGER-SESSION-IDS", session_id.to_owned())
    }

    /// Gemeinsamer Request-Kern (curl_easy_perform + Fehler-Mapping:
    /// CURLE_HTTP_RETURNED_ERROR → HttpNonok, sonst Network).
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<&str>,
        timeout: Duration,
    ) -> ChiakiResult<String> {
        let mut req = self.agent.request(method, url).timeout(timeout);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        let result = match body {
            Some(b) => req.send_string(b),
            None => req.call(),
        };
        match result {
            Ok(resp) => {
                let code = resp.status();
                let body = resp.into_string().unwrap_or_default();
                if code != 200 {
                    tracing::error!(
                        "{} {}: failed with HTTP code {}",
                        method,
                        url,
                        code
                    );
                    return Err(ChiakiError::HttpNonok);
                }
                Ok(body)
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                tracing::error!(
                    "{} {}: failed with HTTP code {}",
                    method,
                    url,
                    code
                );
                tracing::trace!("Response Body: {}.", body);
                Err(ChiakiError::HttpNonok)
            }
            Err(e) => {
                tracing::error!("{} {}: failed with transport error {}", method, url, e);
                Err(ChiakiError::Network)
            }
        }
    }

    /// Port von `chiaki_holepunch_list_devices()`. Nur PS5 (wie im C).
    pub fn list_devices(&self, console_type: ConsoleType) -> ChiakiResult<Vec<DeviceInfo>> {
        if console_type != ConsoleType::Ps5 {
            tracing::warn!("Only PS5 is supported by the list devices function!");
            return Err(ChiakiError::InvalidData);
        }
        let url = DEVICE_LIST_URL_FMT.replace("{platform}", "PS5");

        let (hk, hv) = self.oauth_header();
        let body = self.request(
            "GET",
            &url,
            &[
                ("Accept-Language", "jp".to_owned()),
                (hk, hv),
            ],
            None,
            Duration::from_secs(5),
        )?;

        let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            tracing::error!("chiaki_holepunch_list_devices: Parsing JSON failed: {e}");
            ChiakiError::Unknown
        })?;

        let clients = json
            .get("clients")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                tracing::error!(
                    "chiaki_holepunch_list_devices: JSON does not contain a \"clients\" array field"
                );
                ChiakiError::Unknown
            })?;

        tracing::trace!(
            "chiaki_holepunch_list_devices: retrieved devices \n{}",
            serde_json::to_string_pretty(clients).unwrap_or_default()
        );

        let mut devices = Vec::with_capacity(clients.len());
        for client in clients {
            let duid_str = client
                .get("duid")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tracing::error!("chiaki_holepunch_list_devices: JSON does not contain \"duid\" string field");
                    ChiakiError::Unknown
                })?;
            let mut device_uid = [0u8; 32];
            hex_to_bytes(duid_str, &mut device_uid).map_err(|_| {
                tracing::error!("chiaki_holepunch_list_devices: Could not convert duid to bytes");
                ChiakiError::Unknown
            })?;

            let device_json = client.get("device").ok_or_else(|| {
                tracing::error!("chiaki_holepunch_list_devices: JSON does not contain \"device\" field");
                ChiakiError::Unknown
            })?;

            let mut remoteplay_enabled = false;
            if let Some(features) = device_json.get("enabledFeatures").and_then(|v| v.as_array()) {
                for feature in features {
                    if feature.as_str() == Some("remotePlay") {
                        remoteplay_enabled = true;
                        break;
                    }
                }
            } else {
                tracing::error!("chiaki_holepunch_list_devices: JSON does not contain \"enabledFeatures\" array field");
                return Err(ChiakiError::Unknown);
            }

            let device_name = device_json
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tracing::error!("chiaki_holepunch_list_devices: JSON does not contain \"name\" string field");
                    ChiakiError::Unknown
                })?
                .to_owned();

            devices.push(DeviceInfo {
                type_: console_type,
                device_name,
                device_uid,
                remoteplay_enabled,
            });
        }
        Ok(devices)
    }

    /// Port von `get_websocket_fqdn()`: FQDN des PSN-Push-Notification-
    /// WebSocket-Servers.
    pub fn get_websocket_fqdn(&self) -> ChiakiResult<String> {
        let (hk, hv) = self.oauth_header();
        let body = self.request(
            "GET",
            WS_FQDN_API_URL,
            &[(hk, hv)],
            None,
            Duration::from_secs(10),
        )?;
        let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            tracing::error!("get_websocket_fqdn: Parsing JSON failed: {e}");
            ChiakiError::Unknown
        })?;
        json.get("fqdn")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
            .ok_or_else(|| {
                tracing::error!("get_websocket_fqdn: JSON does not contain \"fqdn\" string field");
                ChiakiError::Unknown
            })
    }

    /// Port von `http_create_session()`.
    ///
    /// @return (sessionId, accountId)
    pub fn create_session(&self, pushctx_id: &str) -> ChiakiResult<(String, i64)> {
        let session_create_json = session_create_json(pushctx_id);
        tracing::trace!("http_create_session: Sending JSON:\n{}", session_create_json);

        let (hk, hv) = self.oauth_header();
        let body = self.request(
            "POST",
            SESSION_CREATE_URL,
            &[(hk, hv), ("Content-Type", JSON_CONTENT_TYPE.to_owned())],
            Some(&session_create_json),
            Duration::from_secs(10),
        )?;

        tracing::trace!("http_create_session: Received JSON:\n{}", body);
        let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            tracing::error!("http_create_session: Parsing JSON failed: {e}");
            ChiakiError::Unknown
        })?;

        let session_id = json
            .pointer("/remotePlaySessions/0/sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                tracing::error!("http_create_session: Unexpected JSON schema, could not parse sessionId and accountId.");
                ChiakiError::Unknown
            })?;
        if session_id.len() != 36 {
            tracing::error!(
                "http_create_session: Unexpected JSON schema, sessionId is not a UUIDv4, was '{}'.",
                session_id
            );
            return Err(ChiakiError::Unknown);
        }
        let account_id = json
            .pointer("/remotePlaySessions/0/members/0/accountId")
            .and_then(|v| match v {
                serde_json::Value::Number(n) => n.as_i64(),
                serde_json::Value::String(s) => s.parse().ok(),
                _ => None,
            })
            .ok_or_else(|| {
                tracing::error!("http_create_session: Unexpected JSON schema, could not parse sessionId and accountId.");
                ChiakiError::Unknown
            })?;

        Ok((session_id.to_owned(), account_id))
    }

    /// Port von `http_check_session()` (Antwort wird nur geloggt, wie im C).
    pub fn check_session(&self, session_id: &str, viewurl: bool) -> ChiakiResult<()> {
        let (hk, hv) = self.oauth_header();
        let (sk, sv) = Self::session_id_header(session_id);
        let body = self.request(
            "GET",
            if viewurl { SESSION_VIEW_URL } else { SESSION_CREATE_URL },
            &[(hk, hv), (sk, sv)],
            None,
            Duration::from_secs(10),
        )?;
        tracing::trace!(
            "http_check_session: retrieved session data \n{}",
            serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or(body)
        );
        Ok(())
    }

    /// Port von `http_start_session()` (PS5, cloudAssistedNavigation-Command).
    pub fn start_session(
        &self,
        console_uid: &[u8; 32],
        console_type: ConsoleType,
        account_id: i64,
        session_id: &str,
        data1: &[u8; 16],
        data2: &[u8; 16],
    ) -> ChiakiResult<()> {
        use chiaki_core::base64;

        let data1_base64 = base64::encode(data1);
        let data2_base64 = base64::encode(data2);
        let payload = session_start_payload(
            account_id,
            session_id,
            &data1_base64,
            &data2_base64,
        );
        let device_uid_str = bytes_to_hex(console_uid);
        let envelope = session_start_envelope(
            &device_uid_str,
            &payload,
            if console_type == ConsoleType::Ps4 { "PS4" } else { "PS5" },
        );

        let (hk, hv) = self.oauth_header();
        tracing::trace!("http_start_session: Sending JSON:\n{}", envelope);
        let body = self.request(
            "POST",
            SESSION_COMMAND_URL,
            &[
                (hk, hv),
                ("Content-Type", JSON_CONTENT_TYPE.to_owned()),
                ("User-Agent", USER_AGENT_RPNET.to_owned()),
            ],
            Some(&envelope),
            Duration::from_secs(10),
        )?;
        tracing::trace!("http_start_session: Received JSON:\n{}", body);
        Ok(())
    }

    /// Port von `http_send_session_message()` (Payload wird vom Aufrufer
    /// serialisiert; Envelope hier).
    pub fn send_session_message(
        &self,
        session_id: &str,
        console_uid: &[u8; 32],
        console_type: ConsoleType,
        account_id: i64,
        payload: &str,
    ) -> ChiakiResult<()> {
        let url = SESSION_MESSAGE_URL_FMT.replace("{session_id}", session_id);
        let console_uid_str = bytes_to_hex(console_uid);
        let msg = session_message_envelope(
            payload,
            account_id,
            &console_uid_str,
            if console_type == ConsoleType::Ps4 { "PS4" } else { "PS5" },
        );
        tracing::trace!("Message to send: {}", msg);

        let (hk, hv) = self.oauth_header();
        self.request(
            "POST",
            &url,
            &[(hk, hv), ("Content-Type", JSON_CONTENT_TYPE.to_owned())],
            Some(&msg),
            Duration::from_secs(10),
        )?;
        Ok(())
    }

    /// Port von `deleteSession()`.
    pub fn delete_session(&self, session_id: &str) -> ChiakiResult<()> {
        let url = DELETE_MESSAGE_URL_FMT.replace("{session_id}", session_id);
        let (hk, hv) = self.oauth_header();
        self.request(
            "DELETE",
            &url,
            &[(hk, hv), ("Content-Type", JSON_CONTENT_TYPE.to_owned())],
            None,
            Duration::from_secs(10),
        )?;
        Ok(())
    }

    /// Port von `http_ps4_session_wakeup()` (Profil-URL holen, dann Wakeup
    /// an die Main-PS4 des Accounts).
    pub fn ps4_session_wakeup(
        &self,
        online_id: &str,
        session_id: &str,
        data1: &[u8; 16],
        data2: &[u8; 16],
    ) -> ChiakiResult<()> {
        use chiaki_core::base64;

        // Schritt 1: user profile base URL
        let (hk, hv) = self.oauth_header();
        let body = self.request(
            "GET",
            USER_PROFILE_URL,
            &[
                (hk, hv),
                ("Connection", "Keep-Alive".to_owned()),
                ("Content-Type", JSON_CONTENT_TYPE.to_owned()),
                ("User-Agent", USER_AGENT_RPNET.to_owned()),
            ],
            None,
            Duration::from_secs(10),
        )?;
        tracing::trace!("http_ps4_session_wakeup: Received JSON:\n{}", body);

        let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            tracing::error!("http_ps4_session_wakeup: Parsing JSON failed: {e}");
            ChiakiError::Unknown
        })?;
        let profile_url = json
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                tracing::error!("http_ps4_session_wakeup: Unexpected JSON schema, could not parse user profile url");
                ChiakiError::Unknown
            })?;

        // C: Base-URL extrahieren (Scheme, Pfad ab '/', Query ab '?', Fragment ab '#')
        let host_url = profile_url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        let host_url = host_url.split(['/', '?', '#']).next().unwrap_or(host_url);

        // Schritt 2: wakeup
        let url = WAKEUP_URL_FMT
            .replace("{base}", profile_url)
            .replace("{online_id}", online_id);

        let data1_base64 = base64::encode(data1);
        let data2_base64 = base64::encode(data2);
        let envelope = session_wakeup_envelope(&data1_base64, &data2_base64, session_id);

        let (hk, hv) = self.oauth_header();
        tracing::trace!("http_ps4_session_wakeup: Sending JSON:\n{}", envelope);
        let body = self.request(
            "POST",
            &url,
            &[
                (hk, hv),
                ("Host", host_url.to_owned()),
                ("Connection", "Keep-Alive".to_owned()),
                ("Content-Type", JSON_CONTENT_TYPE.to_owned()),
                ("User-Agent", USER_AGENT_RPNET.to_owned()),
            ],
            Some(&envelope),
            Duration::from_secs(10),
        )?;
        tracing::trace!("http_ps4_session_wakeup: Received JSON:\n{}", body);
        Ok(())
    }

    /// Port von `get_stun_servers()`, IPv4-Liste (max. 10 Einträge, wie im C).
    pub fn fetch_stun_servers(&self) -> ChiakiResult<Vec<crate::stun::StunServer>> {
        let body = self.request(
            "GET",
            STUN_HOSTS_URL,
            &[],
            None,
            Duration::from_secs(10),
        )?;
        Ok(parse_stun_server_list(&body, 10))
    }

    /// Port von `get_stun_servers()`, IPv6-Liste ("[host]:port"-Zeilen).
    pub fn fetch_stun_servers_ipv6(&self) -> ChiakiResult<Vec<crate::stun::StunServer>> {
        let body = self.request(
            "GET",
            STUN_HOSTS_URL_IPV6,
            &[],
            None,
            Duration::from_secs(10),
        )?;
        Ok(parse_stun_server_list_ipv6(&body, 10))
    }
}

/// Zeilen "host:port" → StunServer (max. `max` Einträge).
fn parse_stun_server_list(body: &str, max: usize) -> Vec<crate::stun::StunServer> {
    let mut out = Vec::new();
    for line in body.lines() {
        if out.len() >= max {
            break;
        }
        if line.is_empty() {
            continue; // strtok überspringt leere Tokens
        }
        let Some((host, port_str)) = line.rsplit_once(':') else {
            tracing::warn!("Problem reading stun server list host");
            break;
        };
        match port_str.parse::<u16>() {
            Ok(port) => out.push(crate::stun::StunServer::new(host, port)),
            Err(_) => {
                tracing::warn!("Problem reading stun server list port");
                break;
            }
        }
    }
    out
}

/// Zeilen "[host]:port" → StunServer (führende '[' entfällt, ':' nach ']').
fn parse_stun_server_list_ipv6(body: &str, max: usize) -> Vec<crate::stun::StunServer> {
    let mut out = Vec::new();
    for line in body.lines() {
        if out.len() >= max {
            break;
        }
        // omit leading [
        let line = line.strip_prefix('[').unwrap_or(line);
        let Some((host, port_str)) = line.split_once(']') else {
            tracing::warn!("Problem reading stun server list host");
            break;
        };
        let port_str = port_str.strip_prefix(':').unwrap_or(port_str);
        match port_str.parse::<u16>() {
            Ok(port) => out.push(crate::stun::StunServer::new(host, port)),
            Err(_) => {
                tracing::warn!("Problem reading stun server list port");
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_create_json_golden() {
        assert_eq!(
            session_create_json("push-ctx-uuid"),
            "{\"remotePlaySessions\":[{\"members\":[{\"accountId\":\"me\",\"deviceUniqueId\":\"me\",\"platform\":\"me\",\"pushContexts\":[{\"pushContextId\":\"push-ctx-uuid\"}]}]}]}"
        );
    }

    #[test]
    fn session_start_payload_golden() {
        let payload = session_start_payload(1234567890123, "sess-uuid", "AAA=", "BBB=");
        assert_eq!(
            payload,
            "{\\\"accountId\\\":1234567890123,\\\"roomId\\\":0,\\\"sessionId\\\":\\\"sess-uuid\\\",\\\"clientType\\\":\\\"Windows\\\",\\\"data1\\\":\\\"AAA=\\\",\\\"data2\\\":\\\"BBB=\\\"}"
        );
    }

    #[test]
    fn session_start_envelope_exact() {
        let payload = session_start_payload(42, "S", "D1", "D2");
        let envelope = session_start_envelope("UID", &payload, "PS4");
        assert_eq!(
            envelope,
            concat!(
                r#"{"commandDetail":{"commandType":"remotePlay","duid":"UID","messageDestination":"SQS","parameters":{"initialParams":""#,
                r#"{\"accountId\":42,\"roomId\":0,\"sessionId\":\"S\",\"clientType\":\"Windows\",\"data1\":\"D1\",\"data2\":\"D2\"}"#,
                r#""},"platform":"PS4"}}"#
            )
        );
    }

    #[test]
    fn session_wakeup_envelope_golden() {
        // Beachte das Leerzeichen nach "roomId": (kaputtes-JSON-Emulation wie im C)
        assert_eq!(
            session_wakeup_envelope("AAA=", "BBB=", "sess"),
            "{\"data\":{\"clientType\":\"Windows\",\"data1\":\"AAA=\",\"data2\":\"BBB=\",\"roomId\": 0,\"protocolVer\":\"10.0\",\"sessionId\":\"sess\"},\"dataTypeSuffix\":\"remotePlay\"}"
        );
    }

    #[test]
    fn session_message_envelope_golden() {
        assert_eq!(
            session_message_envelope("{escaped}", 111222333, "cafe", "PS5"),
            "{\"channel\":\"remote_play:1\",\"payload\":\"ver=1.0, type=text, body={escaped}\",\"to\":[{\"accountId\":\"111222333\",\"deviceUniqueId\":\"cafe\",\"platform\":\"PS5\"}]}"
        );
    }

    #[test]
    fn session_message_json_golden() {
        assert_eq!(
            session_message_json("OFFER", 7, 0, "{...}"),
            "{\\\"action\\\":\\\"OFFER\\\",\\\"reqId\\\":7,\\\"error\\\":0,\\\"connRequest\\\":{...}}"
        );
    }

    #[test]
    fn connrequest_and_candidate_golden() {
        let candidate = session_connrequest_candidate_json("STUN", "1.2.3.4", "0.0.0.0", 9295, 0);
        assert_eq!(
            candidate,
            "{\\\"type\\\":\\\"STUN\\\",\\\"addr\\\":\\\"1.2.3.4\\\",\\\"mappedAddr\\\":\\\"0.0.0.0\\\",\\\"port\\\":9295,\\\"mappedPort\\\":0}"
        );
        let conn = session_connrequest_json(
            4000,
            5000,
            "c2tleQ==",
            2,
            &format!("[{}]", candidate),
            &session_localpeeraddr_json(111222333, "REMOTE_PLAY"),
            "aGFzaA==",
        );
        assert_eq!(
            conn,
            concat!(
                "{\\\"sid\\\":4000,\\\"peerSid\\\":5000,\\\"skey\\\":\\\"c2tleQ==\\\",\\\"natType\\\":2,",
                "\\\"candidate\\\":[{\\\"type\\\":\\\"STUN\\\",\\\"addr\\\":\\\"1.2.3.4\\\",\\\"mappedAddr\\\":\\\"0.0.0.0\\\",\\\"port\\\":9295,\\\"mappedPort\\\":0}],",
                "\\\"defaultRouteMacAddr\\\":\\\"\\\",",
                "\\\"localPeerAddr\\\":{\\\"accountId\\\":\\\"111222333\\\",\\\"platform\\\":\\\"REMOTE_PLAY\\\"},",
                "\\\"localHashedId\\\":\\\"aGFzaA==\\\"}"
            )
        );
    }

    #[test]
    fn short_message_json_golden() {
        assert_eq!(
            session_message_json("RESULT", 3, 0, "{}"),
            "{\\\"action\\\":\\\"RESULT\\\",\\\"reqId\\\":3,\\\"error\\\":0,\\\"connRequest\\\":{}}"
        );
    }

    #[test]
    fn action_str_matches_c() {
        use crate::holepunch::*;
        assert_eq!(action_str(SESSION_MESSAGE_ACTION_OFFER), "OFFER");
        assert_eq!(action_str(SESSION_MESSAGE_ACTION_ACCEPT), "ACCEPT");
        assert_eq!(action_str(SESSION_MESSAGE_ACTION_TERMINATE), "TERMINATE");
        assert_eq!(action_str(SESSION_MESSAGE_ACTION_RESULT), "RESULT");
        assert_eq!(action_str(0), "UNKNOWN");
    }

    #[test]
    fn url_templates_match_c() {
        assert_eq!(
            DEVICE_LIST_URL_FMT.replace("{platform}", "PS5"),
            "https://web.np.playstation.com/api/cloudAssistedNavigation/v2/users/me/clients?platform=PS5&includeFields=device&limit=10&offset=0"
        );
        assert_eq!(SESSION_CREATE_URL, "https://web.np.playstation.com/api/sessionManager/v1/remotePlaySessions");
        assert_eq!(SESSION_VIEW_URL, "https://web.np.playstation.com/api/sessionManager/v1/remotePlaySessions?view=v1.0");
        assert_eq!(
            SESSION_MESSAGE_URL_FMT.replace("{session_id}", "abc"),
            "https://web.np.playstation.com/api/sessionManager/v1/remotePlaySessions/abc/sessionMessage"
        );
        assert_eq!(
            DELETE_MESSAGE_URL_FMT.replace("{session_id}", "abc"),
            "https://web.np.playstation.com/api/sessionManager/v1/remotePlaySessions/abc/members/me"
        );
        assert_eq!(
            WAKEUP_URL_FMT
                .replace("{base}", "https://asm.example.com/base")
                .replace("{online_id}", "me"),
            "https://asm.example.com/base/v1/users/me/remoteConsole/wakeUp?platform=PS4"
        );
        assert_eq!(USER_AGENT_RPNET, "RpNetHttpUtilImpl");
    }

    #[test]
    fn oauth_and_session_id_headers() {
        let client = PsnClient::new("tok123");
        assert_eq!(client.oauth_header(), ("Authorization", "Bearer tok123".to_owned()));
        assert_eq!(
            PsnClient::session_id_header("sess"),
            ("X-PSN-SESSION-MANAGER-SESSION-IDS", "sess".to_owned())
        );
    }

    #[test]
    fn hex_helpers_roundtrip() {
        let mut out = [0u8; 4];
        assert_eq!(hex_to_bytes("deadbeef", &mut out), Ok(()));
        assert_eq!(out, [0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(bytes_to_hex(&out), "deadbeef");
        assert_eq!(hex_to_bytes("zz", &mut out), Err(ChiakiError::InvalidData));
        // Länger als Zielpuffer → wird geklemmt (wie im C)
        let mut small = [0u8; 2];
        assert_eq!(hex_to_bytes("deadbeef", &mut small), Ok(()));
        assert_eq!(small, [0xde, 0xad]);
    }

    #[test]
    fn device_list_parsing() {
        // Zusammengebauter clients[]-Ausschnitt wie er von der API kommt
        let body = r#"{"clients":[
            {"duid":"0000000700410080aabbccddeeff00112233445566778899aabbccddeeff0011",
             "device":{"enabledFeatures":["remotePlay","other"],"name":"PS5-807"}},
            {"duid":"00","device":{"enabledFeatures":["other"],"name":"NoRP"}}
        ]}"#;
        // list_devices() selbst braucht Netz — hier nur die Parse-Logik über
        // denselben Codepfad testen: Wir analysieren manuell mit denselben
        // Regeln wie list_devices.
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        let clients = json.get("clients").unwrap().as_array().unwrap();
        assert_eq!(clients.len(), 2);

        let mut device_uid = [0u8; 32];
        hex_to_bytes(clients[0]["duid"].as_str().unwrap(), &mut device_uid).unwrap();
        assert_eq!(device_uid[0], 0x00);
        assert_eq!(device_uid[3], 0x07);
        assert_eq!(device_uid[7], 0x80);
        assert_eq!(device_uid[8], 0xaa);

        let features = clients[0]["device"]["enabledFeatures"].as_array().unwrap();
        let rp = features.iter().any(|f| f.as_str() == Some("remotePlay"));
        assert!(rp);
        assert_eq!(clients[0]["device"]["name"].as_str().unwrap(), "PS5-807");

        let features2 = clients[1]["device"]["enabledFeatures"].as_array().unwrap();
        assert!(!features2.iter().any(|f| f.as_str() == Some("remotePlay")));
    }

    #[test]
    fn stun_server_list_parsing() {
        let body = "stun.example.org:3478\nstun2.example.net:19302\nbadline\nstun3.example.com:notaport\nstun4.example.com:1";
        let servers = parse_stun_server_list(body, 10);
        assert_eq!(servers.len(), 2, "bricht bei ungültiger Zeile ab (wie im C)");
        assert_eq!(servers[0], crate::stun::StunServer::new("stun.example.org", 3478));
        assert_eq!(servers[1], crate::stun::StunServer::new("stun2.example.net", 19302));

        // max 10
        let many = (0..15).map(|i| format!("h{}.example:{}", i, 1000 + i)).collect::<Vec<_>>().join("\n");
        let servers = parse_stun_server_list(&many, 10);
        assert_eq!(servers.len(), 10);
    }

    #[test]
    fn stun_server_list_ipv6_parsing() {
        let body = "[2001:db8::1]:3478\n[2001:db8::2]:19302";
        let servers = parse_stun_server_list_ipv6(body, 10);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0], crate::stun::StunServer::new("2001:db8::1", 3478));
        assert_eq!(servers[1], crate::stun::StunServer::new("2001:db8::2", 19302));
    }
}
