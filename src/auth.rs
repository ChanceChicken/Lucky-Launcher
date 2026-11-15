//! Microsoft / Xbox / Minecraft sign-in (device-code flow).
//!
//! Chain: Microsoft device code -> Microsoft token -> Xbox Live token -> XSTS token
//!        -> Minecraft access token -> Minecraft profile (name + uuid).
//!
//! You need your OWN Azure "client id" for this (see `SETUP_HELP`). Lucky stores it in
//! `auth.json` in the data folder (or reads the LUCKY_CLIENT_ID environment variable).

use crate::config::Paths;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEVICE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const SCOPE: &str = "XboxLive.signin offline_access";

pub const SETUP_HELP: &str = "\
To sign in you need your own (free) Azure app registration:
  1. Go to https://portal.azure.com -> Microsoft Entra ID -> App registrations -> New registration.
  2. Name it anything (e.g. Lucky Launcher). Supported account types: \"Personal Microsoft accounts only\".
  3. After creating it: Authentication -> add platform \"Mobile and desktop applications\"
     (tick the nativeclient redirect), then set \"Allow public client flows\" to Yes and save.
  4. Copy the \"Application (client) ID\" from the Overview page.
  5. Run: lucky login --client-id <that id>
Mojang must also approve the app for Minecraft access: https://aka.ms/AppRegInfo
Until it is approved, sign-in fails at the last step with \"Invalid app registration\".";

const APPROVAL_HELP: &str = "Microsoft/Mojang rejected the app registration (\"Invalid app registration\"). \
Your Azure app has to be approved for Minecraft access first: see https://aka.ms/AppRegInfo. \
This can take a while, and until then only offline mode works.";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Account {
    pub username: String,
    /// Real Minecraft profile id (no dashes - that is what the game expects).
    pub uuid: String,
    pub mc_token: String,
    /// Unix seconds when `mc_token` stops working.
    pub mc_expires_at: u64,
    pub refresh_token: String,
}

pub struct DeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    pub device_code: String,
    pub interval: u64,
    pub expires_in: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn account_path(paths: &Paths) -> PathBuf {
    paths.root.join("account.json")
}

fn config_path(paths: &Paths) -> PathBuf {
    paths.root.join("auth.json")
}

pub fn client_id(paths: &Paths) -> Option<String> {
    if let Ok(id) = std::env::var("LUCKY_CLIENT_ID") {
        if !id.trim().is_empty() {
            return Some(id.trim().to_string());
        }
    }
    let data = std::fs::read(config_path(paths)).ok()?;
    let v: Value = serde_json::from_slice(&data).ok()?;
    v["client_id"]
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save_client_id(paths: &Paths, id: &str) -> Result<()> {
    let id = id.trim();
    let ok = !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if !ok {
        bail!("that doesn't look like an Azure client id (it should look like 12345678-abcd-....)");
    }
    std::fs::write(config_path(paths), serde_json::to_vec_pretty(&json!({ "client_id": id }))?)?;
    Ok(())
}

pub fn load_account(paths: &Paths) -> Option<Account> {
    serde_json::from_slice(&std::fs::read(account_path(paths)).ok()?).ok()
}

fn save_account(paths: &Paths, acc: &Account) -> Result<()> {
    std::fs::write(account_path(paths), serde_json::to_vec_pretty(acc)?)?;
    Ok(())
}

pub fn logout(paths: &Paths) -> Result<()> {
    match std::fs::remove_file(account_path(paths)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ------------------------------------------------------------------ http helpers

async fn post_json(client: &Client, url: &str, body: Value) -> Result<(u16, Value)> {
    let resp = client
        .post(url)
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Ok((status, v))
}

async fn token_request(client: &Client, form: &[(&str, &str)]) -> Result<Value> {
    let resp = client
        .post(TOKEN_URL)
        .form(form)
        .send()
        .await
        .context("could not reach Microsoft")?;
    resp.json().await.context("unexpected reply from Microsoft")
}

fn ms_error(v: &Value) -> String {
    let code = v["error"].as_str().unwrap_or("error");
    let desc = v["error_description"].as_str().unwrap_or("").lines().next().unwrap_or("");
    let mut msg = format!("Microsoft said {code}: {desc}");
    if desc.contains("client_secret") || desc.contains("client_assertion") {
        msg.push_str("\nHint: in your Azure app, Authentication -> \"Allow public client flows\" must be set to Yes.");
    }
    if desc.contains("AADSTS700016") {
        msg.push_str("\nHint: that client id doesn't exist. Check it, and that the app allows personal Microsoft accounts.");
    }
    msg
}

fn xsts_error(code: Option<u64>) -> String {
    match code {
        Some(2148916233) => "This Microsoft account has no Xbox profile. Sign in once at xbox.com to create one, then try again.".into(),
        Some(2148916235) => "Xbox Live isn't available in your country/region.".into(),
        Some(2148916236) | Some(2148916237) => "This Xbox account needs adult verification before it can sign in.".into(),
        Some(2148916238) => "This is a child account: an adult has to add it to a Microsoft Family first.".into(),
        Some(c) => format!("Xbox Live refused the sign-in (error {c})."),
        None => "Xbox Live refused the sign-in.".into(),
    }
}

// ------------------------------------------------------------------ the chain

async fn minecraft_login(client: &Client, ms_access: &str, ms_refresh: &str) -> Result<Account> {
    let (st, xbl) = post_json(
        client,
        "https://user.auth.xboxlive.com/user/authenticate",
        json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={ms_access}")
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        }),
    )
    .await?;
    if st != 200 {
        bail!("Xbox Live sign-in failed ({st}): {xbl}");
    }
    let xbl_token = xbl["Token"].as_str().context("Xbox Live returned no token")?.to_string();

    let (st, xsts) = post_json(
        client,
        "https://xsts.auth.xboxlive.com/xsts/authorize",
        json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [xbl_token] },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT"
        }),
    )
    .await?;
    if st == 401 {
        bail!("{}", xsts_error(xsts["XErr"].as_u64()));
    }
    if st != 200 {
        bail!("Xbox security token request failed ({st}): {xsts}");
    }
    let xsts_token = xsts["Token"].as_str().context("XSTS returned no token")?;
    let uhs = xsts["DisplayClaims"]["xui"][0]["uhs"]
        .as_str()
        .context("XSTS returned no user hash")?;

    let (st, mc) = post_json(
        client,
        "https://api.minecraftservices.com/authentication/login_with_xbox",
        json!({ "identityToken": format!("XBL3.0 x={uhs};{xsts_token}") }),
    )
    .await?;
    if st != 200 {
        if mc.to_string().contains("Invalid app registration") {
            bail!("{APPROVAL_HELP}");
        }
        bail!("Minecraft sign-in failed ({st}): {mc}");
    }
    let mc_token = mc["access_token"].as_str().context("Minecraft returned no token")?.to_string();
    let expires_in = mc["expires_in"].as_u64().unwrap_or(86_400);

    let resp = client
        .get("https://api.minecraftservices.com/minecraft/profile")
        .bearer_auth(&mc_token)
        .send()
        .await
        .context("could not fetch the Minecraft profile")?;
    let st = resp.status().as_u16();
    let profile: Value = resp.json().await.unwrap_or(Value::Null);
    if st == 404 {
        bail!("This Microsoft account doesn't own Minecraft: Java Edition (or has no profile yet).");
    }
    if st != 200 {
        bail!("Fetching the Minecraft profile failed ({st}): {profile}");
    }
    Ok(Account {
        username: profile["name"].as_str().context("profile has no name")?.to_string(),
        uuid: profile["id"].as_str().context("profile has no id")?.to_string(),
        mc_token,
        mc_expires_at: now() + expires_in,
        refresh_token: ms_refresh.to_string(),
    })
}

/// Full interactive sign-in. `on_code` is called once with the code the user must enter.
pub async fn login_device<F>(
    client: &Client,
    paths: &Paths,
    client_id: &str,
    on_code: F,
) -> Result<Account>
where
    F: Fn(&DeviceCode) + Send,
{
    let v: Value = client
        .post(DEVICE_URL)
        .form(&[("client_id", client_id), ("scope", SCOPE)])
        .send()
        .await
        .context("could not reach Microsoft")?
        .json()
        .await
        .context("unexpected reply from Microsoft")?;
    if v.get("error").is_some() {
        bail!("{}", ms_error(&v));
    }
    let d = DeviceCode {
        user_code: v["user_code"].as_str().context("no user_code")?.to_string(),
        verification_uri: v["verification_uri"]
            .as_str()
            .unwrap_or("https://www.microsoft.com/link")
            .to_string(),
        device_code: v["device_code"].as_str().context("no device_code")?.to_string(),
        interval: v["interval"].as_u64().unwrap_or(5).max(1),
        expires_in: v["expires_in"].as_u64().unwrap_or(900),
    };
    on_code(&d);

    let deadline = now() + d.expires_in;
    let mut interval = d.interval;
    let tokens = loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if now() > deadline {
            bail!("the sign-in code expired - start again");
        }
        let t = token_request(
            client,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", &d.device_code),
            ],
        )
        .await?;
        match t["error"].as_str() {
            None => break t,
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("authorization_declined") => bail!("sign-in was declined"),
            Some("expired_token") => bail!("the sign-in code expired - start again"),
            Some(_) => bail!("{}", ms_error(&t)),
        }
    };

    let access = tokens["access_token"].as_str().context("no access token")?;
    let refresh = tokens["refresh_token"].as_str().context("no refresh token")?;
    let acc = minecraft_login(client, access, refresh).await?;
    save_account(paths, &acc)?;
    Ok(acc)
}

/// The saved account with a working Minecraft token (refreshing it if it's about to expire).
/// `Ok(None)` means nobody is signed in.
pub async fn fresh_account(client: &Client, paths: &Paths) -> Result<Option<Account>> {
    let Some(acc) = load_account(paths) else {
        return Ok(None);
    };
    if acc.mc_expires_at > now() + 300 {
        return Ok(Some(acc));
    }
    let cid = client_id(paths).context("no Azure client id configured (run `lucky login`)")?;
    let t = token_request(
        client,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &cid),
            ("refresh_token", &acc.refresh_token),
            ("scope", SCOPE),
        ],
    )
    .await?;
    if t.get("error").is_some() {
        bail!("your sign-in has expired, run `lucky login` again ({})", ms_error(&t));
    }
    let access = t["access_token"].as_str().context("no access token")?;
    let refresh = t["refresh_token"].as_str().unwrap_or(&acc.refresh_token).to_string();
    let new = minecraft_login(client, access, &refresh).await?;
    save_account(paths, &new)?;
    Ok(Some(new))
}
