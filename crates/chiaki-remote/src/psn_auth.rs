// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// PSN-OAuth-Authentifizierung — Port von gui/src/psntoken.cpp,
// gui/src/psnaccountid.cpp und gui/src/jsonrequester.cpp (chiaki-ng).
//
// Flows (exakt wie im C++):
//
// 1. Login (PSNAccountID::GetPsnAccountId / PSNToken::InitPsnToken):
//    Der User meldet sich im PSN-Login-Webview ([`psn_login_url`]) an; die
//    Redirect-URL auf [`PSN_REDIRECT_PAGE`] enthält den OAuth-Authorization-
//    Code als Query-Parameter `code`. Dieser wird gegen ein Access-/Refresh-
//    Token getauscht (grant_type=authorization_code).
// 2. Refresh (PSNToken::RefreshPsnToken): grant_type=refresh_token.
// 3. Account-ID (PSNAccountID::handUserIDResponse): GET auf
//    `{TOKEN_URL}/{access_token}` liefert `user_id`, die als 8 Bytes
//    Little-Endian (→ Base64) in die Settings wandert.
//
// HTTP (JsonRequester.cpp → ureq): Header exakt wie im C++ nur
// `Authorization` (Basic base64(client_id:client_secret) bzw. Bearer) und
// `Content-Type`. Ein eigener User-Agent wird im C++ nicht gesetzt
// (QNetworkAccessManager sendet seinen eigenen) — auch hier nicht. Timeout
// 10 s wie `kRequestTimeoutMs`. Pro Request wird ein frischer Agent gebaut
// (im C++ wird pro Request ein neues JsonRequester/QNetworkAccessManager
// erzeugt).
//
// Abweichung vom C++ (bewusst): QJsonDocument::fromJson liefert bei kaputtem
// JSON ein leeres Dokument, woraufhin das C++ stillschweigend leere Tokens
// speichert. Hier wird stattdessen ein Fehler gemeldet (Result überall).
//
// Testabdeckung: Request-Bodies/URLs/Headers als Golden-Werte gegen die
// C++-Strings, Response-Parsing mit synthetischen JSONs, Expiry-Berechnung.
// KEINE Netzwerk-Calls in Tests.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chiaki_core::error::{ChiakiError, ChiakiResult};

// ---------------------------------------------------------------------------
// Konstanten — Port des `namespace PSNAuth` (gui/include/psnaccountid.h)
// ---------------------------------------------------------------------------

/// PSNAuth::CLIENT_ID
pub const PSN_CLIENT_ID: &str = "ba495a24-818c-472b-b12d-ff231c1b5745";
/// PSNAuth::CLIENT_SECRET
pub const PSN_CLIENT_SECRET: &str = "mvaiZkRsAsI1IBkY";

/// PSNAuth::TOKEN_URL — OAuth2-Token-Endpunkt (Token-Tausch und Refresh).
pub const PSN_TOKEN_URL: &str = "https://auth.api.sonyentertainmentnetwork.com/2.0/oauth/token";

/// PSNAuth::REDIRECT_PAGE — die Login-Session landet hier nach erfolgreichem
/// Login; die `code`-Query-Parameter dieser URL sind der Authorization-Code.
pub const PSN_REDIRECT_PAGE: &str = "https://remoteplay.dl.playstation.net/remoteplay/redirect";

/// redirect_uri-Parameter der Token-Requests (identischer String wie
/// REDIRECT_PAGE, im C++ beide male hartkodiert).
pub const PSN_REDIRECT_URI: &str = "https://remoteplay.dl.playstation.net/remoteplay/redirect";

/// Scope-Parameter der Token-Requests (aus den QString-Templates).
pub const PSN_SCOPE: &str = "psn:clientapp referenceDataService:countryConfig.read pushNotification:webSocket.desktop.connect sessionManager:remotePlaySession.system.update";

/// PSNAuth::LOGIN_URL — PSN-Login-Seite für das Webview. Die Spaces im
/// `scope`-Parameter sind wie im C++ Original Teil des Templates
/// (QUrl/`toEncoded()` kodiert sie beim Laden als %20).
pub const PSN_LOGIN_URL: &str = "https://auth.api.sonyentertainmentnetwork.com/2.0/oauth/authorize?service_entity=urn:service-entity:psn&response_type=code&client_id=ba495a24-818c-472b-b12d-ff231c1b5745&redirect_uri=https://remoteplay.dl.playstation.net/remoteplay/redirect&scope=psn:clientapp referenceDataService:countryConfig.read pushNotification:webSocket.desktop.connect sessionManager:remotePlaySession.system.update&request_locale=en_US&ui=pr&service_logo=ps&layout_type=popup&smcid=remoteplay&prompt=always&PlatformPrivacyWs1=minimal&";

/// kRequestTimeoutMs aus jsonrequester.cpp.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const JSON_CONTENT_TYPE: &str = "application/json";

// ---------------------------------------------------------------------------
// Request-Bau (1:1 die QString-Templates aus psntoken.cpp/psnaccountid.cpp)
// ---------------------------------------------------------------------------

/// Port von `PSNToken::InitPsnToken`-Body
/// ("grant_type=authorization_code&code=%1&scope=...&redirect_uri=...&").
pub fn authorization_code_body(redirect_code: &str) -> String {
    format!(
        "grant_type=authorization_code&code={redirect_code}&scope={PSN_SCOPE}&redirect_uri={PSN_REDIRECT_URI}&"
    )
}

/// Port von `PSNToken::RefreshPsnToken`-Body
/// ("grant_type=refresh_token&refresh_token=%1&scope=...&redirect_uri=...&").
pub fn refresh_token_body(refresh_token: &str) -> String {
    format!(
        "grant_type=refresh_token&refresh_token={refresh_token}&scope={PSN_SCOPE}&redirect_uri={PSN_REDIRECT_URI}&"
    )
}

/// Port von `QmlBackend::psnLoginUrl()` (qmlbackend.cpp):
/// `PSNAuth::LOGIN_URL + "duid=" + duid + "&"`.
pub fn psn_login_url(duid: &str) -> String {
    format!("{PSN_LOGIN_URL}duid={duid}&")
}

/// Port von `PSNAccountID::handleAccessTokenResponse`:
/// `QString("%1/%2").arg(TOKEN_URL).arg(access_token)`.
pub fn account_info_url(access_token: &str) -> String {
    format!("{PSN_TOKEN_URL}/{access_token}")
}

/// Port von `JsonRequester::generateBasicAuthHeader`:
/// "Basic " + base64("username:password").
pub fn generate_basic_auth_header(username: &str, password: &str) -> String {
    use chiaki_core::base64;
    format!("Basic {}", base64::encode(format!("{username}:{password}").as_bytes()))
}

/// Port von `JsonRequester::generateBearerAuthHeader`: "Bearer <token>".
pub fn generate_bearer_auth_header(bearer_token: &str) -> String {
    format!("Bearer {bearer_token}")
}

/// Basic-Auth-Header mit den PSN-App-Credentials (wird von beiden C++-
/// Klassen im Konstruktor erzeugt).
pub fn psn_basic_auth_header() -> String {
    generate_basic_auth_header(PSN_CLIENT_ID, PSN_CLIENT_SECRET)
}

// ---------------------------------------------------------------------------
// Token-Response (PSNToken::handleAccessTokenResponse)
// ---------------------------------------------------------------------------

/// Ergebnis des Token-Tausch/Refresh-Requests. Enthält exakt die Werte, die
/// das C++ in die Settings schreibt (access_token, refresh_token,
/// expires_in) plus den daraus berechneten Ablauf-Zeitstempel
/// (C++: `QDateTime::currentDateTime().addSecs(expires_in)` — hier als
/// Unix-Sekunden).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshedPsnToken {
    pub access_token: String,
    pub refresh_token: String,
    /// `expires_in` der PSN-API (Sekunden).
    pub expires_in: u64,
    /// Ablauf als Unix-Zeitstempel (Sekunden): `now + expires_in`.
    pub expires_at_unix: u64,
}

/// Port von `handleAccessTokenResponse`: JSON parsen und Expiry berechnen.
///
/// `now_unix` wird als Parameter übernommen, damit die Expiry-Berechnung
/// testbar ist (Aufrufer: [`now_unix`]).
pub fn parse_token_response(body: &str, now_unix: u64) -> ChiakiResult<RefreshedPsnToken> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        tracing::error!("psn_auth: Parsing token JSON failed: {e}");
        ChiakiError::Unknown
    })?;

    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            tracing::error!("psn_auth: token JSON does not contain \"access_token\" string field");
            ChiakiError::InvalidData
        })?;
    let refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            tracing::error!("psn_auth: token JSON does not contain \"refresh_token\" string field");
            ChiakiError::InvalidData
        })?;
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .filter(|s| *s >= 0)
        .ok_or_else(|| {
            tracing::error!("psn_auth: token JSON does not contain a valid \"expires_in\" field");
            ChiakiError::InvalidData
        })?;

    // C++: QDateTime expiry = currentTime.addSecs(secondsLeft);
    Ok(RefreshedPsnToken {
        access_token: access_token.to_owned(),
        refresh_token: refresh_token.to_owned(),
        expires_in: expires_in as u64,
        expires_at_unix: now_unix + expires_in as u64,
    })
}

// ---------------------------------------------------------------------------
// Account-ID (PSNAccountID::handUserIDResponse / to_bytes_little_endian)
// ---------------------------------------------------------------------------

/// Port von `PSNAccountID::to_bytes_little_endian(number, 8)`: die user_id
/// als 8 Bytes Little-Endian (so erwartet es das Remote-Play-Protokoll bzw.
/// `settings->SetPsnAccountId(byte_representation.toBase64())`).
pub fn user_id_to_bytes_le(user_id: i64) -> [u8; 8] {
    user_id.to_le_bytes()
}

/// Port von `handUserIDResponse`: JSON mit `user_id` parsen (die API liefert
/// ihn als String; numerische Werte werden ebenso akzeptiert) und in die 8
/// Account-Bytes wandeln.
pub fn parse_account_id(body: &str) -> ChiakiResult<[u8; 8]> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        tracing::error!("psn_auth: Parsing account info JSON failed: {e}");
        ChiakiError::Unknown
    })?;

    // C++: object.value("user_id").toString() + std::stoll — der Wert kommt
    // von der API als String; eine Zahl wird hier zusätzlich zugelassen.
    let user_id = match json.get("user_id") {
        Some(serde_json::Value::String(s)) => s.parse::<i64>().map_err(|_| ChiakiError::InvalidData),
        Some(serde_json::Value::Number(n)) => n.as_i64().ok_or(ChiakiError::InvalidData),
        _ => Err(ChiakiError::InvalidData),
    }
    .map_err(|_| {
        tracing::error!("psn_auth: account info JSON does not contain a valid \"user_id\" field");
        ChiakiError::InvalidData
    })?;

    Ok(user_id_to_bytes_le(user_id))
}

// ---------------------------------------------------------------------------
// HTTP-Kern (jsonrequester.cpp → ureq)
// ---------------------------------------------------------------------------

/// Port von `JsonRequester::makeRequest`: POST/GET mit `Authorization`- und
/// `Content-Type`-Header, 10-s-Timeout, Antwort-Body als String.
/// Fehler-Mapping wie in psn.rs: HTTP != 2xx → `HttpNonok`, Transportfehler
/// → `Network`.
fn make_request(
    post: bool,
    url: &str,
    auth_header: &str,
    content_type: &str,
    body: Option<&str>,
) -> ChiakiResult<String> {
    let agent = ureq::AgentBuilder::new().build();
    let mut req = agent.request(if post { "POST" } else { "GET" }, url);
    req = req.timeout(REQUEST_TIMEOUT);
    req = req.set("Authorization", auth_header);
    req = req.set("Content-Type", content_type);

    let result = match body {
        Some(b) => req.send_string(b),
        None => req.call(),
    };
    match result {
        Ok(resp) => Ok(resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => {
            let resp_body = resp.into_string().unwrap_or_default();
            tracing::error!(
                "[E] : Url ({url}) returned error ({resp_body}) with error code ({code})"
            );
            // C++: ProtocolInvalidOperationError (HTTP 400, z.B. abgelaufener
            // Refresh-Token) löst zusätzlich UnauthorizedError aus. Das
            // ChiakiError-Enum hat dafür keinen eigenen Wert — der Aufrufer
            // behandelt HttpNonok beim Refresh als "Credentials abgelaufen".
            Err(ChiakiError::HttpNonok)
        }
        Err(e) => {
            tracing::error!("{url}: failed with transport error {e}");
            Err(ChiakiError::Network)
        }
    }
}

/// Aktuelle Unix-Zeit in Sekunden (für `expires_at_unix`).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Öffentliche API (PSNToken::InitPsnToken / PSNToken::RefreshPsnToken /
// PSNAccountID::GetPsnAccountId als blockierende Funktionen)
// ---------------------------------------------------------------------------

/// Port von `PSNToken::InitPsnToken`: Authorization-Code (aus der
/// Redirect-URL des PSN-Logins) gegen Access-/Refresh-Token tauschen.
pub fn exchange_authorization_code(redirect_code: &str) -> ChiakiResult<RefreshedPsnToken> {
    let body = authorization_code_body(redirect_code);
    tracing::trace!("psn_auth: exchanging authorization code, body:\n{body}");
    let response = make_request(
        true,
        PSN_TOKEN_URL,
        &psn_basic_auth_header(),
        FORM_CONTENT_TYPE,
        Some(&body),
    )?;
    tracing::trace!("psn_auth: token response:\n{response}");
    parse_token_response(&response, now_unix())
}

/// Port von `PSNToken::RefreshPsnToken`: Access-Token mit Refresh-Token
/// erneuern.
pub fn refresh_psn_token(refresh_token: &str) -> ChiakiResult<RefreshedPsnToken> {
    let body = refresh_token_body(refresh_token);
    tracing::trace!("psn_auth: refreshing PSN token, body:\n{body}");
    let response = make_request(
        true,
        PSN_TOKEN_URL,
        &psn_basic_auth_header(),
        FORM_CONTENT_TYPE,
        Some(&body),
    )?;
    tracing::trace!("psn_auth: token response:\n{response}");
    parse_token_response(&response, now_unix())
}

/// Port von `PSNAccountID::GetPsnAccountId` (zweiter Schritt,
/// `handUserIDResponse`): mit dem Access-Token die PSN-Account-ID holen
/// (8 Bytes Little-Endian; für die Settings als Base64 kodieren — siehe
/// chiaki-settings `account_id_to_b64`).
pub fn fetch_psn_account_id(access_token: &str) -> ChiakiResult<[u8; 8]> {
    let url = account_info_url(access_token);
    let response = make_request(false, &url, &psn_basic_auth_header(), JSON_CONTENT_TYPE, None)?;
    tracing::trace!("psn_auth: account info response:\n{response}");
    parse_account_id(&response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chiaki_core::base64;

    #[test]
    fn constants_match_cpp() {
        assert_eq!(PSN_CLIENT_ID, "ba495a24-818c-472b-b12d-ff231c1b5745");
        assert_eq!(PSN_CLIENT_SECRET, "mvaiZkRsAsI1IBkY");
        assert_eq!(
            PSN_TOKEN_URL,
            "https://auth.api.sonyentertainmentnetwork.com/2.0/oauth/token"
        );
        assert_eq!(
            PSN_REDIRECT_PAGE,
            "https://remoteplay.dl.playstation.net/remoteplay/redirect"
        );
        assert_eq!(PSN_REDIRECT_URI, PSN_REDIRECT_PAGE);
        assert_eq!(
            format!("{PSN_TOKEN_URL}/xyz"),
            "https://auth.api.sonyentertainmentnetwork.com/2.0/oauth/token/xyz"
        );
    }

    #[test]
    fn login_url_matches_cpp() {
        // PSNAuth::LOGIN_URL aus psnaccountid.h (ohne duid-Suffix)
        assert_eq!(
            PSN_LOGIN_URL,
            "https://auth.api.sonyentertainmentnetwork.com/2.0/oauth/authorize?\
service_entity=urn:service-entity:psn&response_type=code&\
client_id=ba495a24-818c-472b-b12d-ff231c1b5745&\
redirect_uri=https://remoteplay.dl.playstation.net/remoteplay/redirect&\
scope=psn:clientapp referenceDataService:countryConfig.read \
pushNotification:webSocket.desktop.connect sessionManager:remotePlaySession.system.update&\
request_locale=en_US&ui=pr&service_logo=ps&layout_type=popup&smcid=remoteplay&\
prompt=always&PlatformPrivacyWs1=minimal&"
        );
    }

    #[test]
    fn psn_login_url_appends_duid() {
        // QmlBackend::psnLoginUrl(): LOGIN_URL + "duid=" + duid + "&"
        let url = psn_login_url("AABBCCDD");
        assert!(url.starts_with(PSN_LOGIN_URL));
        assert!(url.ends_with("&duid=AABBCCDD&"));
        assert_eq!(url, format!("{PSN_LOGIN_URL}duid=AABBCCDD&"));
    }

    #[test]
    fn authorization_code_body_golden() {
        // QString("grant_type=authorization_code&code=%1&scope=...\
        // &redirect_uri=https://remoteplay.dl.playstation.net/remoteplay/redirect&")
        //     .arg(redirectCode) aus psntoken.cpp/psnaccountid.cpp
        assert_eq!(
            authorization_code_body("v3.RedirectCode.abc"),
            "grant_type=authorization_code&code=v3.RedirectCode.abc&\
scope=psn:clientapp referenceDataService:countryConfig.read \
pushNotification:webSocket.desktop.connect sessionManager:remotePlaySession.system.update&\
redirect_uri=https://remoteplay.dl.playstation.net/remoteplay/redirect&"
        );
    }

    #[test]
    fn refresh_token_body_golden() {
        // QString("grant_type=refresh_token&refresh_token=%1&scope=...\
        // &redirect_uri=...&").arg(refreshToken) aus psntoken.cpp
        assert_eq!(
            refresh_token_body("rt.v4.Xyz"),
            "grant_type=refresh_token&refresh_token=rt.v4.Xyz&\
scope=psn:clientapp referenceDataService:countryConfig.read \
pushNotification:webSocket.desktop.connect sessionManager:remotePlaySession.system.update&\
redirect_uri=https://remoteplay.dl.playstation.net/remoteplay/redirect&"
        );
    }

    #[test]
    fn basic_auth_header_golden() {
        // JsonRequester::generateBasicAuthHeader(CLIENT_ID, CLIENT_SECRET)
        assert_eq!(
            psn_basic_auth_header(),
            "Basic YmE0OTVhMjQtODE4Yy00NzJiLWIxMmQtZmYyMzFjMWI1NzQ1Om12YWlaa1JzQXNJMUlCa1k="
        );
        assert_eq!(
            generate_basic_auth_header("user", "pw"),
            format!("Basic {}", base64::encode(b"user:pw"))
        );
    }

    #[test]
    fn bearer_auth_header_golden() {
        assert_eq!(
            generate_bearer_auth_header("tok123"),
            "Bearer tok123".to_owned()
        );
    }

    #[test]
    fn account_info_url_golden() {
        // QString("%1/%2").arg(PSNAuth::TOKEN_URL).arg(access_token)
        assert_eq!(
            account_info_url("at.abc.def"),
            "https://auth.api.sonyentertainmentnetwork.com/2.0/oauth/token/at.abc.def"
        );
    }

    #[test]
    fn parse_token_response_golden() {
        // Synthetische OAuth2-Antwort (Feldnamen wie in
        // handleAccessTokenResponse)
        let body = r#"{"access_token":"v4.at.abc","token_type":"bearer","expires_in":3600,"refresh_token":"v4.rt.def","scope":"psn:clientapp"}"#;
        let token = parse_token_response(body, 1_000_000).expect("parse");
        assert_eq!(token.access_token, "v4.at.abc");
        assert_eq!(token.refresh_token, "v4.rt.def");
        assert_eq!(token.expires_in, 3600);
        assert_eq!(token.expires_at_unix, 1_003_600, "expiry = now + expires_in");
    }

    #[test]
    fn parse_token_response_zero_expiry() {
        let body = r#"{"access_token":"a","refresh_token":"r","expires_in":0}"#;
        let token = parse_token_response(body, 42).expect("parse");
        assert_eq!(token.expires_at_unix, 42);
    }

    #[test]
    fn parse_token_response_errors() {
        // Kaputtes JSON → Unknown (C++ speichert hier stillschweigend leere
        // Tokens — siehe Modul-Kommentar)
        assert_eq!(
            parse_token_response("not json", 0).unwrap_err(),
            ChiakiError::Unknown
        );
        // Fehlende Felder → InvalidData
        assert_eq!(
            parse_token_response(r#"{"access_token":"a"}"#, 0).unwrap_err(),
            ChiakiError::InvalidData
        );
        assert_eq!(
            parse_token_response(
                r#"{"access_token":"a","refresh_token":"r","expires_in":-5}"#,
                0
            )
            .unwrap_err(),
            ChiakiError::InvalidData
        );
    }

    #[test]
    fn user_id_to_bytes_le_golden() {
        // Account-ID aus den chiaki-settings-Tests: "eVr/5uFEAHE="
        let user_id: i64 = 8142583863319681657;
        let bytes = user_id_to_bytes_le(user_id);
        assert_eq!(bytes, [0x79, 0x5a, 0xff, 0xe6, 0xe1, 0x44, 0x00, 0x71]);
        assert_eq!(base64::encode(&bytes), "eVr/5uFEAHE=");

        // Trivialwerte
        assert_eq!(user_id_to_bytes_le(1), [1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(user_id_to_bytes_le(0), [0; 8]);
    }

    #[test]
    fn parse_account_id_string_golden() {
        // Die Account-Info-API liefert user_id als String (C++:
        // object.value("user_id").toString() + std::stoll)
        let body = r#"{"country":"US","jti":"...","user_id":"8142583863319681657"}"#;
        let bytes = parse_account_id(body).expect("parse");
        assert_eq!(bytes, [0x79, 0x5a, 0xff, 0xe6, 0xe1, 0x44, 0x00, 0x71]);
        assert_eq!(base64::encode(&bytes), "eVr/5uFEAHE=");
    }

    #[test]
    fn parse_account_id_numeric() {
        let body = r#"{"user_id":8142583863319681657}"#;
        let bytes = parse_account_id(body).expect("parse");
        assert_eq!(bytes, [0x79, 0x5a, 0xff, 0xe6, 0xe1, 0x44, 0x00, 0x71]);
    }

    #[test]
    fn parse_account_id_errors() {
        assert_eq!(parse_account_id("}{").unwrap_err(), ChiakiError::Unknown);
        assert_eq!(
            parse_account_id(r#"{"user_id":"notanumber"}"#).unwrap_err(),
            ChiakiError::InvalidData
        );
        assert_eq!(
            parse_account_id(r#"{"other":"field"}"#).unwrap_err(),
            ChiakiError::InvalidData
        );
    }
}
