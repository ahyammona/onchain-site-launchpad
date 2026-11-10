//! Gateway: GET /<site-account-address> -> reads the account over RPC,
//! verifies the SHA-256 in the header, and serves the page.
//!
//! env:
//!   RPC_URL     default http://127.0.0.1:8899
//!   PROGRAM_ID  only accounts owned by this program are served (required)
//!   PORT        default 8080
//!   MODE        both (default) | launchpad (UI + API only) | sites (published pages only).
//!               Run two deployments, one of each, so pages never share an origin with the launchpad.

#[path = "../launchpad.rs"]
mod launchpad;

use layout::{decompress, read_site, ENC_BROTLI};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::{
    collections::HashMap,
    env,
    str::FromStr,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const CACHE_TTL: Duration = Duration::from_secs(15);

struct Cached {
    at: Instant,
    encoding: u8,
    payload: Vec<u8>,
    etag: String,
}

struct State {
    rpc: RpcClient,
    program_id: Pubkey,
    cache: Mutex<HashMap<Pubkey, Arc<Cached>>>,
}

enum Failure {
    NotFound,
    Status(u16, &'static str),
    Internal(String),
}

fn load(state: &State, address: Pubkey) -> Result<Arc<Cached>, Failure> {
    if let Some(hit) = state.cache.lock().unwrap().get(&address) {
        if hit.at.elapsed() < CACHE_TTL {
            return Ok(hit.clone());
        }
    }
    let acct = state
        .rpc
        .get_account_with_commitment(&address, CommitmentConfig::confirmed())
        .map_err(|e| Failure::Internal(e.to_string()))?
        .value
        .ok_or(Failure::NotFound)?;
    if acct.owner != state.program_id {
        return Err(Failure::Status(
            403,
            "account is not owned by the site program",
        ));
    }
    let site =
        read_site(&acct.data).map_err(|_| Failure::Status(422, "account is not a valid site"))?;
    if !site.hash_ok {
        return Err(Failure::Status(502, "stored bytes do not match their hash"));
    }
    let etag = format!(
        "\"{}\"",
        site.header
            .sha256
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let entry = Arc::new(Cached {
        at: Instant::now(),
        encoding: site.header.encoding,
        payload: site.payload.to_vec(),
        etag,
    });
    state.cache.lock().unwrap().insert(address, entry.clone());
    Ok(entry)
}

fn h(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
}

fn text(req: Request, code: u16, msg: &str) {
    let _ = req.respond(
        Response::from_string(format!("{msg}\n"))
            .with_status_code(StatusCode(code))
            .with_header(h("Content-Type", "text/plain; charset=utf-8")),
    );
}

fn mode() -> String {
    env::var("MODE").unwrap_or_else(|_| "both".into())
}

fn handle(state: &State, req: Request) {
    let mode = mode();
    let req = if mode == "sites" {
        req
    } else {
        match launchpad::route(&state.rpc, &state.program_id, req) {
            Some(r) => r,
            None => return,
        }
    };
    if mode == "launchpad" {
        return text(req, 404, "this is the launchpad; published sites are served from a separate address");
    }
    if !matches!(req.method(), Method::Get | Method::Head) {
        return text(req, 405, "method not allowed");
    }
    let path = req
        .url()
        .split('?')
        .next()
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();
    let first = path.split('/').next().unwrap_or("");
    if first.is_empty() {
        return text(req, 404, "not found");
    }
    let address = match Pubkey::from_str(first) {
        Ok(a) => a,
        Err(_) => return text(req, 400, "not a valid account address"),
    };

    let site = match load(state, address) {
        Ok(s) => s,
        Err(Failure::NotFound) => return text(req, 404, "no such account"),
        Err(Failure::Status(code, msg)) => return text(req, code, msg),
        Err(Failure::Internal(e)) => {
            eprintln!("rpc error: {e}");
            return text(req, 502, "gateway error");
        }
    };

    let header_value = |name: &str| {
        req.headers()
            .iter()
            .find(|x| x.field.as_str().as_str().eq_ignore_ascii_case(name))
            .map(|x| x.value.as_str().to_string())
            .unwrap_or_default()
    };

    if header_value("If-None-Match") == site.etag {
        let _ = req.respond(
            Response::empty(304)
                .with_header(h("ETag", &site.etag))
                .with_header(h("Access-Control-Allow-Origin", "*")),
        );
        return;
    }

    let mut body = site.payload.clone();
    let mut encoded_br = false;
    if site.encoding == ENC_BROTLI {
        let accepts_br = header_value("Accept-Encoding")
            .split(',')
            .any(|p| p.trim().split(';').next() == Some("br"));
        if accepts_br {
            encoded_br = true; // browser decompresses
        } else {
            body = match decompress(&body, site.encoding) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("decompress failed: {e}");
                    return text(req, 500, "gateway error");
                }
            };
        }
    }

    let is_head = *req.method() == Method::Head;
    let mut resp = Response::from_data(if is_head { Vec::new() } else { body })
        .with_header(h("Content-Type", "text/html; charset=utf-8"))
        .with_header(h("ETag", &site.etag))
        .with_header(h("Cache-Control", "public, max-age=15"))
        .with_header(h("X-Content-Type-Options", "nosniff"))
        .with_header(h("Access-Control-Allow-Origin", "*"))
        .with_header(h(
            "Content-Security-Policy",
            "default-src 'self' 'unsafe-inline' data:",
        ));
    if encoded_br {
        resp = resp
            .with_header(h("Content-Encoding", "br"))
            .with_header(h("Vary", "Accept-Encoding"));
    }
    let _ = req.respond(resp);
}

fn main() {
    let program_id = match env::var("PROGRAM_ID")
        .ok()
        .and_then(|s| Pubkey::from_str(&s).ok())
    {
        Some(p) => p,
        None => {
            eprintln!("set PROGRAM_ID to the deployed program address");
            std::process::exit(1);
        }
    };
    if !matches!(mode().as_str(), "both" | "launchpad" | "sites") {
        eprintln!("MODE must be both, launchpad or sites");
        std::process::exit(1);
    }
    // Hosts that only offer environment variables (no secret files) can pass the program wallet
    // as KEYPAIR_JSON (the contents of the keypair file). It is written to a private temp file
    // and exposed as KEYPAIR, which is what the launchpad and the `upload` helper read.
    if let Ok(json) = env::var("KEYPAIR_JSON") {
        let path = env::temp_dir().join("launch-keypair.json");
        {
            use std::io::Write;
            #[cfg(unix)]
            use std::os::unix::fs::OpenOptionsExt;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            opts.mode(0o600);
            match opts.open(&path).and_then(|mut f| f.write_all(json.trim().as_bytes())) {
                Ok(()) => env::set_var("KEYPAIR", &path),
                Err(e) => {
                    eprintln!("cannot write KEYPAIR_JSON to {}: {e}", path.display());
                    std::process::exit(1);
                }
            }
        }
        env::remove_var("KEYPAIR_JSON");
    }
    let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8899".into());
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let state = Arc::new(State {
        rpc: RpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed()),
        program_id,
        cache: Mutex::new(HashMap::new()),
    });
    let server = Server::http(("0.0.0.0", port)).expect("cannot bind port");
    println!("gateway on http://localhost:{port}  mode={}  rpc={rpc_url}  program={program_id}", mode());

    for req in server.incoming_requests() {
        let state = state.clone();
        thread::spawn(move || handle(&state, req));
    }
}
