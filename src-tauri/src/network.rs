//! Iroh wiring for file-drop.
//!
//! Lifecycle per connection:
//!   1. ALPN handshake. We accept two: MAIN (the real protocol) and PING
//!      (presence probe — we just close it immediately).
//!   2. For MAIN: open a single "card" bi-stream and exchange one card
//!      each.
//!   3. Keep the connection alive as a Session. Each subsequent file
//!      transfer opens its own bi-stream: OFFER → ACCEPT/REJECT → bytes.
//!
//! Files land in `$XDG_DOWNLOAD_DIR/krill-file-drop/` (or
//! `~/Downloads/krill-file-drop/`), never overwriting — append " (2)" etc.
//! on name collisions.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use data_encoding::BASE32_NOPAD;
use iroh::endpoint::presets;
use iroh::{
    endpoint::Connection, Endpoint, EndpointAddr, EndpointId, PublicKey, SecretKey,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex};

pub const MAIN_ALPN: &[u8] = b"krill-file-drop/v1";
pub const PING_ALPN: &[u8] = b"krill-file-drop/ping/v1";
const PRESENCE_INTERVAL: Duration = Duration::from_secs(15);
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(5);

// ---- Wire-level types ---------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Card {
    pub version: u32,
    #[serde(rename = "nodeId", default)]
    pub node_id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub icon: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Contact {
    #[serde(rename = "nodeId")]
    pub node_id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub icon: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(rename = "lastPaired")]
    pub last_paired: u64, // unix millis
}

#[derive(Serialize, Deserialize, Default)]
struct ContactsFile {
    contacts: Vec<Contact>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Offer {
    name: String,
    size: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct OfferResponse {
    accepted: bool,
}

// ---- Network handle -----------------------------------------------------

#[derive(Clone)]
pub struct Network {
    endpoint: Endpoint,
    our_card: Arc<Mutex<Card>>,
    session: Arc<Mutex<Option<Session>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<bool>>>>,
    next_offer_id: Arc<AtomicU64>,
    contacts: Arc<Mutex<Vec<Contact>>>,
}

#[derive(Clone)]
struct Session {
    conn: Connection,
    peer_id: String,
}

impl Network {
    pub async fn start(secret_key: SecretKey, mut card: Card, app: AppHandle) -> Result<Self> {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(vec![MAIN_ALPN.to_vec(), PING_ALPN.to_vec()])
            .bind()
            .await
            .context("binding iroh endpoint")?;

        card.node_id = endpoint.id().to_string();
        let contacts = load_contacts();
        let me = Self {
            endpoint: endpoint.clone(),
            our_card: Arc::new(Mutex::new(card)),
            session: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_offer_id: Arc::new(AtomicU64::new(1)),
            contacts: Arc::new(Mutex::new(contacts)),
        };

        // Accept loop.
        let net = me.clone();
        let app_a = app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let Some(incoming) = net.endpoint.accept().await else {
                    break;
                };
                let net2 = net.clone();
                let app2 = app_a.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = net2.handle_incoming(incoming, app2).await {
                        eprintln!("[file-drop] incoming failed: {e:?}");
                    }
                });
            }
        });

        // Presence loop.
        let net = me.clone();
        let app_p = app.clone();
        tauri::async_runtime::spawn(async move {
            // Initial delay so the endpoint registers with the relay first.
            tokio::time::sleep(Duration::from_secs(2)).await;
            loop {
                net.run_presence_sweep(&app_p).await;
                tokio::time::sleep(PRESENCE_INTERVAL).await;
            }
        });

        Ok(me)
    }

    pub async fn ticket(&self) -> Result<String> {
        self.endpoint.online().await;
        let addr = self.endpoint.addr();
        encode_ticket(&addr)
    }

    pub fn node_id(&self) -> String {
        self.endpoint.id().to_string()
    }

    pub async fn update_card(&self, mut card: Card) {
        card.node_id = self.node_id();
        *self.our_card.lock().await = card;
    }

    pub async fn list_contacts(&self) -> Vec<Contact> {
        self.contacts.lock().await.clone()
    }

    pub async fn connect_to_ticket(&self, ticket: &str, app: AppHandle) -> Result<Card> {
        let addr = decode_ticket(ticket).context("parsing code")?;
        self.dial_main(addr, app).await
    }

    pub async fn dial_contact(&self, node_id: &str, app: AppHandle) -> Result<Card> {
        let pk = parse_public_key(node_id)?;
        self.dial_main(EndpointAddr::from(pk), app).await
    }

    async fn dial_main(&self, addr: EndpointAddr, app: AppHandle) -> Result<Card> {
        let conn = self
            .endpoint
            .connect(addr, MAIN_ALPN)
            .await
            .context("dialing peer")?;

        // Initiator opens the card bi-stream.
        let (mut send, mut recv) = conn.open_bi().await.context("opening card stream")?;
        let our = self.our_card.lock().await.clone();
        write_framed(&mut send, &serde_json::to_vec(&our)?).await?;
        send.finish().ok();
        let peer_bytes = read_framed(&mut recv, 1 << 20).await?;
        let peer: Card = serde_json::from_slice(&peer_bytes).context("parsing peer card")?;

        self.install_session(peer.clone(), conn, app.clone()).await;
        self.upsert_contact(&peer, &app).await;
        Ok(peer)
    }

    pub async fn disconnect(&self) {
        let mut g = self.session.lock().await;
        if let Some(s) = g.take() {
            s.conn.close(0u32.into(), b"bye");
        }
    }

    pub async fn send_file(&self, path: &Path, app: AppHandle) -> Result<()> {
        let session = self
            .session
            .lock()
            .await
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("not connected"))?;

        let meta = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        let size = meta.len();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());

        let (mut send, mut recv) = session.conn.open_bi().await.context("opening offer stream")?;
        let offer = Offer { name: name.clone(), size };
        write_framed(&mut send, &serde_json::to_vec(&offer)?).await?;

        let peer_id = session.peer_id.clone();
        emit(&app, "send-status", &serde_json::json!({
            "phase": "waiting", "name": name, "size": size, "peerId": peer_id,
        }));

        let resp_bytes = read_framed(&mut recv, 1024).await?;
        let resp: OfferResponse = serde_json::from_slice(&resp_bytes)?;
        if !resp.accepted {
            send.finish().ok();
            emit(&app, "send-status", &serde_json::json!({
                "phase": "rejected", "name": name, "peerId": peer_id,
            }));
            return Ok(());
        }

        emit(&app, "send-status", &serde_json::json!({
            "phase": "sending", "name": name, "size": size, "sent": 0, "peerId": peer_id,
        }));

        let mut file = File::open(path).await
            .with_context(|| format!("opening {}", path.display()))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut sent: u64 = 0;
        let mut last_emit: u64 = 0;
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 { break; }
            send.write_all(&buf[..n]).await?;
            sent += n as u64;
            if sent - last_emit >= 64 * 1024 {
                last_emit = sent;
                emit(&app, "send-status", &serde_json::json!({
                    "phase": "sending", "name": name, "size": size,
                    "sent": sent, "peerId": peer_id,
                }));
            }
        }
        send.finish().ok();
        let _ = recv.read_to_end(0).await;

        emit(&app, "send-status", &serde_json::json!({
            "phase": "done", "name": name, "size": size, "peerId": peer_id,
        }));
        Ok(())
    }

    pub async fn respond_to_offer(&self, offer_id: u64, accept: bool) -> Result<()> {
        let tx = self.pending.lock().await.remove(&offer_id)
            .ok_or_else(|| anyhow!("no such offer"))?;
        let _ = tx.send(accept);
        Ok(())
    }

    // ---- Presence -------------------------------------------------------

    async fn run_presence_sweep(&self, app: &AppHandle) {
        let contacts = self.contacts.lock().await.clone();
        let active_peer = self.session.lock().await.as_ref().map(|s| s.peer_id.clone());
        let mut seen: HashSet<String> = HashSet::new();
        for c in contacts {
            if !seen.insert(c.node_id.clone()) { continue; }
            // Active session = definitely online.
            if active_peer.as_deref() == Some(c.node_id.as_str()) {
                emit(app, "contact-presence", &serde_json::json!({
                    "nodeId": c.node_id, "online": true,
                }));
                continue;
            }
            let pk = match parse_public_key(&c.node_id) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let online = match tokio::time::timeout(
                PRESENCE_TIMEOUT,
                self.endpoint.connect(EndpointAddr::from(pk), PING_ALPN),
            ).await {
                Ok(Ok(conn)) => { conn.close(0u32.into(), b"ping"); true }
                _ => false,
            };
            emit(app, "contact-presence", &serde_json::json!({
                "nodeId": c.node_id, "online": online,
            }));
        }
    }

    // ---- Internals ------------------------------------------------------

    async fn handle_incoming(&self, incoming: iroh::endpoint::Incoming, app: AppHandle) -> Result<()> {
        let conn: Connection = incoming.await.context("completing handshake")?;
        let alpn = conn.alpn().to_vec();
        if alpn == PING_ALPN {
            // Presence probe — peer just wanted to know we're online.
            conn.close(0u32.into(), b"pong");
            return Ok(());
        }
        if alpn != MAIN_ALPN {
            conn.close(0u32.into(), b"unknown alpn");
            return Err(anyhow!("unknown alpn"));
        }

        let (mut send, mut recv) = conn.accept_bi().await.context("accepting card stream")?;
        let our = self.our_card.lock().await.clone();
        write_framed(&mut send, &serde_json::to_vec(&our)?).await?;
        send.finish().ok();
        let peer_bytes = read_framed(&mut recv, 1 << 20).await?;
        let peer: Card = serde_json::from_slice(&peer_bytes).context("parsing peer card")?;

        self.install_session(peer.clone(), conn, app.clone()).await;
        self.upsert_contact(&peer, &app).await;
        Ok(())
    }

    async fn install_session(&self, peer: Card, conn: Connection, app: AppHandle) {
        // Tear down any previous session so we only ever have one live.
        if let Some(prev) = self.session.lock().await.take() {
            prev.conn.close(0u32.into(), b"replaced");
        }
        *self.session.lock().await = Some(Session {
            conn: conn.clone(),
            peer_id: peer.node_id.clone(),
        });
        emit(&app, "session-started", &peer);

        let net = self.clone();
        let app2 = app.clone();
        let peer_id = peer.node_id.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let (send, recv) = match conn.accept_bi().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let net2 = net.clone();
                let app3 = app2.clone();
                let pid = peer_id.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = net2.handle_incoming_offer(send, recv, app3, pid).await {
                        eprintln!("[file-drop] incoming offer failed: {e:?}");
                    }
                });
            }
            // Connection closed.
            {
                let mut g = net.session.lock().await;
                if let Some(s) = g.as_ref() {
                    if s.peer_id == peer_id { *g = None; }
                }
            }
            emit(&app2, "session-ended", &serde_json::json!({ "peerId": peer_id }));
        });
    }

    async fn handle_incoming_offer(
        &self,
        mut send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
        app: AppHandle,
        peer_id: String,
    ) -> Result<()> {
        let offer_bytes = read_framed(&mut recv, 64 * 1024).await?;
        let offer: Offer = serde_json::from_slice(&offer_bytes)?;
        let id = self.next_offer_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<bool>();
        self.pending.lock().await.insert(id, tx);
        emit(&app, "file-offered", &serde_json::json!({
            "offerId": id, "name": offer.name, "size": offer.size, "peerId": peer_id,
        }));

        let accepted = rx.await.unwrap_or(false);
        write_framed(&mut send, &serde_json::to_vec(&OfferResponse { accepted })?).await?;
        if !accepted {
            send.finish().ok();
            return Ok(());
        }

        let dir = downloads_dir();
        tokio::fs::create_dir_all(&dir).await.ok();
        let path = unique_path(&dir, &offer.name);
        let display_name = path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| offer.name.clone());

        let mut file = File::create(&path).await
            .with_context(|| format!("creating {}", path.display()))?;

        let mut got: u64 = 0;
        let mut last_emit: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        while got < offer.size {
            let want = std::cmp::min(buf.len() as u64, offer.size - got) as usize;
            let n = recv.read(&mut buf[..want]).await?.unwrap_or(0);
            if n == 0 { break; }
            file.write_all(&buf[..n]).await?;
            got += n as u64;
            if got - last_emit >= 64 * 1024 {
                last_emit = got;
                emit(&app, "recv-status", &serde_json::json!({
                    "phase": "receiving", "offerId": id, "name": display_name,
                    "size": offer.size, "got": got, "peerId": peer_id,
                }));
            }
        }
        file.flush().await.ok();
        send.finish().ok();

        let phase = if got == offer.size { "done" } else { "partial" };
        emit(&app, "recv-status", &serde_json::json!({
            "phase": phase, "offerId": id, "name": display_name,
            "size": offer.size, "got": got,
            "path": path.display().to_string(), "peerId": peer_id,
        }));
        Ok(())
    }

    async fn upsert_contact(&self, card: &Card, app: &AppHandle) {
        if card.node_id.is_empty() { return; }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut list = self.contacts.lock().await;
        if let Some(existing) = list.iter_mut().find(|c| c.node_id == card.node_id) {
            existing.display_name = card.display_name.clone();
            existing.icon = card.icon.clone();
            existing.avatar = card.avatar.clone();
            existing.last_paired = now;
        } else {
            list.push(Contact {
                node_id: card.node_id.clone(),
                display_name: card.display_name.clone(),
                icon: card.icon.clone(),
                avatar: card.avatar.clone(),
                last_paired: now,
            });
        }
        let snapshot = list.clone();
        drop(list);
        if let Err(e) = save_contacts(&snapshot) {
            eprintln!("[file-drop] saving contacts failed: {e:?}");
        }
        emit(app, "contacts-updated", &snapshot);
    }
}

fn emit<S: serde::Serialize + Clone>(app: &AppHandle, event: &str, payload: &S) {
    if let Err(e) = app.emit(event, payload.clone()) {
        eprintln!("[file-drop] emit {event} failed: {e:?}");
    }
}

// ---- Length-prefixed framing on a stream -------------------------------

async fn write_framed<W: AsyncWriteExt + Unpin>(w: &mut W, bytes: &[u8]) -> Result<()> {
    let len: u32 = bytes.len().try_into().map_err(|_| anyhow!("frame too large"))?;
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(bytes).await?;
    Ok(())
}

async fn read_framed<R: AsyncReadExt + Unpin>(r: &mut R, max: u32) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await.context("reading length")?;
    let len = u32::from_le_bytes(len_buf);
    if len > max {
        return Err(anyhow!("frame length {len} over limit {max}"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await.context("reading body")?;
    Ok(body)
}

// ---- Ticket / NodeId parsing -------------------------------------------

fn encode_ticket(addr: &EndpointAddr) -> Result<String> {
    let bytes = postcard::to_allocvec(addr).context("encoding addr")?;
    Ok(BASE32_NOPAD.encode(&bytes).to_lowercase())
}

fn decode_ticket(s: &str) -> Result<EndpointAddr> {
    let bytes = BASE32_NOPAD
        .decode(s.trim().to_uppercase().as_bytes())
        .context("base32 decode")?;
    let addr: EndpointAddr = postcard::from_bytes(&bytes).context("postcard decode")?;
    Ok(addr)
}

fn parse_public_key(s: &str) -> Result<PublicKey> {
    let _: EndpointId; // type assertion that PublicKey == EndpointId per docs
    PublicKey::from_str(s.trim()).context("parsing node id")
}

// ---- Filesystem ---------------------------------------------------------

fn downloads_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("krill-file-drop")
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() { return candidate; }
    let (stem, ext) = split_name(name);
    for n in 2..1000 {
        let alt = if ext.is_empty() {
            format!("{stem} ({n})")
        } else {
            format!("{stem} ({n}).{ext}")
        };
        let p = dir.join(alt);
        if !p.exists() { return p; }
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis()).unwrap_or(0);
    let alt = if ext.is_empty() { format!("{stem}.{ts}") } else { format!("{stem}.{ts}.{ext}") };
    dir.join(alt)
}

fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

// ---- Contacts persistence ----------------------------------------------

fn contacts_path() -> PathBuf {
    state_dir().join("contacts.json")
}

fn load_contacts() -> Vec<Contact> {
    let path = contacts_path();
    let Ok(bytes) = std::fs::read(&path) else { return Vec::new() };
    match serde_json::from_slice::<ContactsFile>(&bytes) {
        Ok(f) => f.contacts,
        Err(e) => {
            eprintln!("[file-drop] contacts.json malformed: {e:?}");
            Vec::new()
        }
    }
}

fn save_contacts(list: &[Contact]) -> Result<()> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let payload = ContactsFile { contacts: list.to_vec() };
    let bytes = serde_json::to_vec_pretty(&payload)?;
    std::fs::write(contacts_path(), bytes).context("writing contacts.json")?;
    Ok(())
}

// ---- Secret key persistence -------------------------------------------

pub fn load_or_create_secret_key(state_dir: &Path) -> Result<SecretKey> {
    let path = state_dir.join("secret_key");
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            return Ok(SecretKey::from_bytes(&arr));
        }
        eprintln!(
            "[file-drop] {} has unexpected length {}, regenerating",
            path.display(),
            bytes.len()
        );
    }
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;
    let sk = SecretKey::generate();
    let bytes: [u8; 32] = sk.to_bytes();
    write_private(&path, &bytes)?;
    Ok(sk)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true).truncate(true).write(true).mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[derive(Serialize, Debug)]
pub struct ReceivedFile {
    pub name: String,
    pub size: u64,
    pub modified: u64, // unix millis
    pub path: String,
}

pub fn list_received_files() -> Vec<ReceivedFile> {
    let dir = downloads_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<ReceivedFile> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let meta = e.metadata().ok()?;
            if !meta.is_file() { return None; }
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            Some(ReceivedFile {
                name: path.file_name()?.to_string_lossy().to_string(),
                size: meta.len(),
                modified,
                path: path.display().to_string(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

pub fn open_path(path: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .with_context(|| format!("xdg-open {path}"))?;
    Ok(())
}

/// Put a file reference (text/uri-list) onto the clipboard so it can be
/// pasted as a file into a file manager. Uses wl-copy on Wayland,
/// xclip on X11.
pub fn copy_file_to_clipboard(path: &str) -> Result<()> {
    let uri = path_to_file_uri(path);
    let use_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();

    let (cmd, args): (&str, Vec<&str>) = if use_wayland {
        ("wl-copy", vec!["--type", "text/uri-list"])
    } else {
        ("xclip", vec!["-selection", "clipboard", "-t", "text/uri-list"])
    };

    let mut child = std::process::Command::new(cmd)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| {
            if use_wayland {
                "spawning wl-copy (install wl-clipboard?)".to_string()
            } else {
                "spawning xclip (install xclip?)".to_string()
            }
        })?;

    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(uri.as_bytes()).context("writing to clipboard helper")?;
    }
    let status = child.wait().context("waiting on clipboard helper")?;
    if !status.success() {
        return Err(anyhow!("{cmd} exited with {status}"));
    }
    Ok(())
}

/// Encode an absolute path into a `file://` URI, percent-escaping any
/// byte that isn't an unreserved RFC 3986 character or a path separator.
fn path_to_file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for b in path.as_bytes() {
        match *b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/' => out.push(*b as char),
            _ => out.push_str(&format!("%{:02X}", *b)),
        }
    }
    out
}

pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("krill-file-drop")
}
