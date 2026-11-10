//! Launchpad routes for the gateway: the web UI, GET /api/info, POST /api/launch.
//!
//! "Program wallet" mode: /api/launch runs the `upload` binary (sitting next to the
//! gateway binary) with the gateway's own KEYPAIR, so visitors can publish without a wallet.
//!
//! env (in addition to the gateway's):
//!   KEYPAIR          program wallet keypair (default ~/.config/solana/id.json)
//!   PROGRAM_WALLET   set to "off" to disable program-wallet publishing
//!   PUBLIC_RPC_URL   RPC the browser may call for "your wallet" mode. Defaults to RPC_URL
//!                    only when that is localhost. Never put a private API key here.
//!   SITES_URL        public address of the sites-only deployment (MODE=sites). Published links
//!                    point there so pages never share an origin with this launchpad.
//!   MAX_LAUNCHES_PER_HOUR  program-wallet publishes allowed per hour (default 30)

use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
};
use std::{
    env, fs,
    io::Read,
    process::Command,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tiny_http::{Header, Method, Request, Response, StatusCode};

const UI: &str = include_str!("ui.html");
const MAX_HTML: u64 = 256 * 1024;
static BUSY: Mutex<()> = Mutex::new(());
static RECENT: Mutex<Vec<Instant>> = Mutex::new(Vec::new());

fn sites_url() -> Option<String> {
    env::var("SITES_URL").ok().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty())
}

/// Browsers always send `Origin` on a POST. A request from another origin (for example a
/// published page calling this API) is refused. Non-browser clients send no Origin and pass.
pub fn origin_ok(origin: Option<&str>, host: Option<&str>) -> bool {
    match origin {
        None => true,
        Some(o) => match (o.split_once("://"), host) {
            (Some((_, h)), Some(host)) => h.eq_ignore_ascii_case(host),
            _ => false,
        },
    }
}

fn header_of(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn launch_allowed() -> bool {
    let max: usize = env::var("MAX_LAUNCHES_PER_HOUR").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
    let mut v = RECENT.lock().unwrap();
    let now = Instant::now();
    v.retain(|t| now.duration_since(*t) < Duration::from_secs(3600));
    if v.len() >= max {
        return false;
    }
    v.push(now);
    true
}

fn hdr(n: &str, v: &str) -> Header {
    Header::from_bytes(n.as_bytes(), v.as_bytes()).unwrap()
}

fn esc(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 32 => o.push(' '),
            c => o.push(c),
        }
    }
    o
}

fn json(req: Request, code: u16, body: String) {
    let _ = req.respond(
        Response::from_string(body)
            .with_status_code(StatusCode(code))
            .with_header(hdr("Content-Type", "application/json"))
            .with_header(hdr("Cache-Control", "no-store")),
    );
}

fn err(req: Request, code: u16, msg: &str) {
    json(req, code, format!("{{\"error\":\"{}\"}}", esc(msg)));
}

fn keypair_path() -> String {
    env::var("KEYPAIR").unwrap_or_else(|_| {
        format!("{}/.config/solana/id.json", env::var("HOME").unwrap_or_default())
    })
}

fn program_wallet() -> Option<Pubkey> {
    if env::var("PROGRAM_WALLET").map(|v| v == "off").unwrap_or(false) {
        return None;
    }
    read_keypair_file(keypair_path()).ok().map(|k| k.pubkey())
}

fn rpc_url() -> String {
    env::var("RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8899".into())
}

fn public_rpc() -> Option<String> {
    env::var("PUBLIC_RPC_URL").ok().or_else(|| {
        let u = rpc_url();
        (u.contains("127.0.0.1") || u.contains("localhost")).then_some(u)
    })
}

fn cluster() -> &'static str {
    let u = rpc_url();
    if u.contains("devnet") { "devnet" }
    else if u.contains("testnet") { "testnet" }
    else if u.contains("mainnet") { "mainnet" }
    else if u.contains("127.0.0.1") || u.contains("localhost") { "localnet" }
    else { "custom" }
}

fn info(req: Request, rpc: &RpcClient, program_id: &Pubkey) {
    let wallet = match program_wallet() {
        Some(pk) => {
            let sol = rpc.get_balance(&pk).unwrap_or(0) as f64 / 1e9;
            format!("{{\"address\":\"{pk}\",\"sol\":{sol}}}")
        }
        None => "null".into(),
    };
    let pubrpc = public_rpc().map(|u| format!("\"{}\"", esc(&u))).unwrap_or("null".into());
    let sites = sites_url().map(|u| format!("\"{}\"", esc(&u))).unwrap_or("null".into());
    json(
        req,
        200,
        format!(
            "{{\"programId\":\"{program_id}\",\"cluster\":\"{}\",\"programWallet\":{wallet},\"rpc\":{pubrpc},\"sitesUrl\":{sites}}}",
            cluster()
        ),
    );
}

fn launch(mut req: Request) {
    if program_wallet().is_none() {
        return err(req, 403, "the program wallet is not available on this gateway");
    }
    if !origin_ok(header_of(&req, "Origin").as_deref(), header_of(&req, "Host").as_deref()) {
        return err(req, 403, "cross-origin requests to the launchpad are not allowed");
    }
    let mut body = Vec::new();
    if req.as_reader().take(MAX_HTML + 1).read_to_end(&mut body).is_err() || body.is_empty() {
        return err(req, 400, "send the page HTML as the request body");
    }
    if body.len() as u64 > MAX_HTML {
        return err(req, 413, "page is larger than 256 KB; use the CLI uploader");
    }
    if !launch_allowed() {
        return err(req, 429, "publish limit reached for this hour; try again later or use your own wallet");
    }
    let freeze = req.url().contains("freeze=1");
    let _guard = match BUSY.try_lock() {
        Ok(g) => g,
        Err(_) => return err(req, 429, "another publish is running; try again in a moment"),
    };

    let exe = match env::current_exe() {
        Ok(p) => p.with_file_name(if cfg!(windows) { "upload.exe" } else { "upload" }),
        Err(e) => return err(req, 500, &e.to_string()),
    };
    if !exe.exists() {
        return err(req, 500, "upload binary not found next to the gateway; run `cargo build --release`");
    }
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = env::temp_dir().join(format!("launch-{nanos}.html"));
    if let Err(e) = fs::write(&tmp, &body) {
        return err(req, 500, &format!("cannot write temp file: {e}"));
    }
    let mut cmd = Command::new(&exe);
    cmd.arg(&tmp);
    if freeze {
        cmd.arg("--freeze");
    }
    let out = cmd.output();
    let _ = fs::remove_file(&tmp);

    match out {
        Ok(o) if o.status.success() => {
            let log = String::from_utf8_lossy(&o.stdout).to_string();
            let addr = log
                .lines()
                .find_map(|l| l.strip_prefix("site address: "))
                .unwrap_or("")
                .trim()
                .to_string();
            if addr.is_empty() {
                return err(req, 500, "upload finished but no address was reported");
            }
            json(req, 200, format!("{{\"address\":\"{addr}\",\"log\":\"{}\"}}", esc(&log)));
        }
        Ok(o) => {
            let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
            eprintln!("launch failed: {e}");
            err(req, 502, if e.is_empty() { "upload failed" } else { &e });
        }
        Err(e) => err(req, 500, &format!("cannot run upload: {e}")),
    }
}

/// Handles UI and /api routes. Returns the request back if it is not ours.
pub fn route(rpc: &RpcClient, program_id: &Pubkey, req: Request) -> Option<Request> {
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let m = req.method().clone();
    match (m, path.as_str()) {
        (Method::Get, "/") => {
            let csp = format!(
                "default-src 'self' 'unsafe-inline' data:; connect-src 'self' {} {}",
                public_rpc().unwrap_or_default(),
                sites_url().unwrap_or_default()
            );
            let _ = req.respond(
                Response::from_string(UI)
                    .with_header(hdr("Content-Type", "text/html; charset=utf-8"))
                    .with_header(hdr("Content-Security-Policy", &csp))
                    .with_header(hdr("X-Content-Type-Options", "nosniff")),
            );
            None
        }
        (Method::Get, "/api/info") => { info(req, rpc, program_id); None }
        (Method::Post, "/api/launch") => { launch(req); None }
        _ => Some(req),
    }
}

#[cfg(test)]
mod tests {
    use super::origin_ok;

    #[test]
    fn same_origin_passes() {
        assert!(origin_ok(Some("https://app.example.com"), Some("app.example.com")));
        assert!(origin_ok(Some("http://localhost:8080"), Some("localhost:8080")));
        assert!(origin_ok(Some("HTTP://LocalHost:8080"), Some("localhost:8080")));
    }

    #[test]
    fn other_origins_are_refused() {
        assert!(!origin_ok(Some("null"), Some("localhost:8080"))); // sandboxed pages
        assert!(!origin_ok(Some("https://evil.example"), Some("app.example.com")));
        assert!(!origin_ok(Some("http://127.0.0.1:8080"), Some("localhost:8080")));
        assert!(!origin_ok(Some("https://app.example.com"), None));
    }

    #[test]
    fn requests_without_origin_are_allowed() {
        assert!(origin_ok(None, Some("localhost:8080"))); // curl, scripts
    }
}
