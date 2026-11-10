# onchain-site

Host a static page in a Solana account. All Rust; the only JS is inside `site/index.html`.

```
program/   on-chain program (Init / Write / Close / Freeze)
tools/     layout (shared), `upload` and `gateway` binaries
site/      the page itself
```

## Account layout

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `SOLW` |
| 4 | 1 | version (1) |
| 5 | 1 | encoding (0 raw, 1 brotli) |
| 6 | 32 | authority (all zeros = frozen) |
| 38 | 4 | payload length, u32 LE |
| 42 | 32 | sha256 of stored (compressed) payload |
| 74 | n | payload |

`program/src/lib.rs` and `tools/src/layout.rs` must agree on this.

## Instructions (accounts: `[authority (signer), site (writable)]`)

| Tag | Name | Data |
|---|---|---|
| 0 | Init | encoding u8, length u32 LE, sha256 [32] |
| 1 | Write | offset u32 LE, bytes |
| 2 | Close | none (refunds rent to authority) |
| 3 | Freeze | none (zeroes authority, permanent) |

## Run

```sh
# program
cd program && cargo build-sbf
solana program deploy target/deploy/onchain_site.so   # note the program id

# upload
cd ../tools
export PROGRAM_ID=<program id> RPC_URL=<rpc>
cargo run --release --bin upload -- ../site/index.html          # add --freeze to lock it

# serve
cargo run --release --bin gateway
# open http://localhost:8080/<site address>
```

## Notes

- Not compiled or tested yet. Run `cargo check` in `program/` and `tools/` first.
- Writes are ~900 B per tx. A 20 KB compressed page is ~23 txs.
- Rent is a refundable deposit; Close returns it and deletes the site.
- Don't freeze until you have seen the page load through the gateway.
- The gateway only serves accounts owned by `PROGRAM_ID` and checks the hash before serving.

## Web UI (launchpad)

The gateway now serves a publishing UI at `/`. Publish by pasting or opening HTML.

- **Program wallet**: the gateway runs `upload` with its own `KEYPAIR`. No setup for the visitor.
  Disable with `PROGRAM_WALLET=off`. Anyone who can reach the gateway can spend this wallet's SOL
  (pages are capped at 256 KB, one publish at a time), so use a devnet or local wallet only.
- **Your wallet**: any Wallet Standard wallet (Phantom, Solflare, Backpack). The browser builds the
  transactions and the wallet signs them in one approval. Needs `PUBLIC_RPC_URL` (defaults to
  `RPC_URL` when that is localhost). Stored uncompressed (no Brotli in browsers), max 120 write txs.

```sh
cd tools && cargo build --release          # builds gateway AND upload (the UI shells out to upload)
PROGRAM_ID=<id> ./target/release/gateway   # open http://localhost:8080
```

## Hosting it publicly (devnet)

Published pages are arbitrary HTML and JavaScript. Never serve them from the same address as the
launchpad, or a page can call the launchpad's API and prompt visitors' wallets. Run the same image twice:

| | launchpad | sites |
|---|---|---|
| `MODE` | `launchpad` | `sites` |
| serves | the UI and `/api/*` | `/<address>` pages only |
| needs | `KEYPAIR`, `PUBLIC_RPC_URL`, `SITES_URL` | nothing extra |

Both need `PROGRAM_ID` and `RPC_URL`. Set `SITES_URL` on the launchpad to the sites service's public address.
`POST /api/launch` refuses requests from other origins and is capped by `MAX_LAUNCHES_PER_HOUR` (default 30).
Use a dedicated devnet keypair, never committed. Supply it as a secret file (`KEYPAIR=/path`) or, on hosts
with only environment variables, paste the keypair file's contents into `KEYPAIR_JSON`.

```sh
docker build -t onchain-site .        # or let .github/workflows/image.yml publish it to ghcr.io
```

Status: proof of concept on devnet. Unaudited. Anything published is public and permanent.
