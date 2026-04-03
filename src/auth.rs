use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::Duration;

const CLIENT_ID: &str = "groovy-cli-plex-client";
const PRODUCT: &str = "Groovy CLI";

#[derive(Deserialize)]
struct PinResponse {
    id: u64,
    code: String,
    #[serde(rename = "authToken")]
    auth_token: Option<String>,
}

/// Run Plex OAuth flow: request PIN, open browser, poll for token.
pub fn plex_oauth() -> Result<String> {
    let client = reqwest::blocking::Client::new();

    // Step 1: Request a PIN
    eprintln!("Requesting Plex auth PIN...");
    let resp = client
        .post("https://plex.tv/api/v2/pins")
        .header("Accept", "application/json")
        .form(&[
            ("strong", "true"),
            ("X-Plex-Product", PRODUCT),
            ("X-Plex-Client-Identifier", CLIENT_ID),
        ])
        .send()
        .context("Failed to request Plex PIN")?;

    let pin: PinResponse = resp.json().context("Failed to parse PIN response")?;
    eprintln!("Got PIN: {} (id={})", pin.code, pin.id);

    // Step 2: Open browser for user to authorize
    let auth_url = format!(
        "https://app.plex.tv/auth#?clientID={}&code={}&context%5Bdevice%5D%5Bproduct%5D={}",
        CLIENT_ID,
        pin.code,
        urlencoded(PRODUCT),
    );

    eprintln!("\nOpening browser for Plex authorization...");
    eprintln!("If the browser doesn't open, go to:\n  {}\n", auth_url);

    // Try to open browser
    let _ = std::process::Command::new("open")
        .arg(&auth_url)
        .spawn()
        .or_else(|_| std::process::Command::new("xdg-open").arg(&auth_url).spawn());

    // Step 3: Poll for token
    eprintln!("Waiting for authorization (press Ctrl+C to cancel)...");

    for i in 0..120 {
        // 2 minutes max
        std::thread::sleep(Duration::from_secs(1));

        let resp = client
            .get(format!("https://plex.tv/api/v2/pins/{}", pin.id))
            .header("Accept", "application/json")
            .header("X-Plex-Client-Identifier", CLIENT_ID)
            .send()
            .context("Failed to poll PIN")?;

        let status: PinResponse = resp.json().context("Failed to parse poll response")?;

        if let Some(token) = status.auth_token {
            if !token.is_empty() {
                eprintln!("\n✓ Authenticated!");
                println!("{}", token);
                return Ok(token);
            }
        }

        if i % 5 == 4 {
            eprint!(".");
        }
    }

    bail!("Authorization timed out after 2 minutes")
}

fn urlencoded(s: &str) -> String {
    s.replace(' ', "%20")
}
