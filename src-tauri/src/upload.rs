//! Phone → desktop uploads over the LAN.
//!
//! A tiny HTTP server the user starts on demand. The desktop shows a QR
//! code for `http://<lan-ip>:<port>/`; the phone opens that page in its
//! browser and posts files to `/upload`, which streams them into the
//! download folder. No accounts, no relay, nothing leaves the LAN. The
//! server only runs while the user has it switched on.

use anyhow::{anyhow, Result};
use axum::{
    extract::{DefaultBodyLimit, Multipart, State as AxumState},
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Router,
};
use qrcode::{render::svg, QrCode};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// One file that landed via the phone page — handed to the callback so the
/// app can log it like any other received file.
pub struct ReceivedUpload {
    pub name: String,
    pub size: u64,
    pub path: PathBuf,
}

pub type OnReceived = Arc<dyn Fn(ReceivedUpload) + Send + Sync>;

struct ServerState {
    download_dir: PathBuf,
    on_received: OnReceived,
}

/// What the UI needs to show while the server is up.
#[derive(Clone)]
pub struct ServerInfo {
    pub url: String,
    /// Inline SVG of the QR code for `url`.
    pub qr_svg: String,
}

#[derive(Default)]
pub struct UploadServer {
    running: Option<Running>,
}

struct Running {
    info: ServerInfo,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl UploadServer {
    /// Bind `port` on all interfaces and start serving. The app uses a
    /// fixed port so a firewall rule can name it; tests pass 0.
    /// Idempotent: if already running, returns the existing info.
    pub async fn start(
        &mut self,
        port: u16,
        download_dir: PathBuf,
        lan_ip: String,
        on_received: OnReceived,
    ) -> Result<ServerInfo> {
        if let Some(r) = &self.running {
            return Ok(r.info.clone());
        }

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))
            .await
            .map_err(|e| anyhow!("can't listen on port {port}: {e}"))?;
        let port = listener.local_addr()?.port();

        let state = Arc::new(ServerState { download_dir, on_received });
        let app = Router::new()
            .route("/", get(index_page))
            .route("/upload", post(handle_upload))
            // Multipart is streamed to disk chunk by chunk, so the request
            // body can be as large as the phone cares to send.
            .layer(DefaultBodyLimit::disable())
            .with_state(state);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let url = format!("http://{lan_ip}:{port}/");
        let info = ServerInfo { qr_svg: qr_svg(&url)?, url };
        self.running = Some(Running { info: info.clone(), shutdown_tx, task });
        Ok(info)
    }

    pub fn info(&self) -> Option<ServerInfo> {
        self.running.as_ref().map(|r| r.info.clone())
    }

    pub async fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            let _ = r.shutdown_tx.send(());
            let _ = r.task.await;
        }
    }
}

/// QR as inline SVG. Ink on Ghost White regardless of the desktop theme:
/// phone cameras read dark-on-light far more reliably than the inverse,
/// and the code is content the phone scans, not chrome.
fn qr_svg(url: &str) -> Result<String> {
    let code = QrCode::new(url.as_bytes())?;
    let doc = code
        .render::<svg::Color>()
        .min_dimensions(220, 220)
        .quiet_zone(true)
        .dark_color(svg::Color("#30343F"))
        .light_color(svg::Color("#FAFAFF"))
        .build();
    // Drop the `<?xml …?>` prologue: this goes straight into innerHTML.
    let start = doc.find("<svg").unwrap_or(0);
    Ok(doc[start..].to_string())
}

/// Best-effort LAN address: the interface the OS would route to the
/// internet through. `connect` on a UDP socket sends nothing; it only
/// picks the local address. None if there's no route at all.
pub fn lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

async fn index_page() -> Html<&'static str> {
    Html(include_str!("upload.html"))
}

async fn handle_upload(
    AxumState(state): AxumState<Arc<ServerState>>,
    mut multipart: Multipart,
) -> Result<StatusCode, (StatusCode, String)> {
    tokio::fs::create_dir_all(&state.download_dir)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let Some(name) = field.file_name().map(sanitize_name) else { continue };
        let path = crate::network::unique_path(&state.download_dir, &name);
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let mut size: u64 = 0;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            size += chunk.len() as u64;
        }
        file.flush().await.ok();

        let saved_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(name);
        (state.on_received)(ReceivedUpload { name: saved_name, size, path });
    }
    Ok(StatusCode::OK)
}

/// Keep only the final path component of whatever the browser sent, and
/// never let it be empty — the phone controls this string.
fn sanitize_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base == "." || base == ".." {
        "upload".to_string()
    } else {
        base.to_string()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Post one multipart file over a raw socket and return the status line.
    async fn post_file(url: &str, name: &str, body: &[u8]) -> String {
        let hostport = url.trim_start_matches("http://").trim_end_matches('/');
        let mut s = tokio::net::TcpStream::connect(hostport).await.unwrap();
        let boundary = "krillboundary";
        let mut payload = Vec::new();
        payload.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        payload.extend_from_slice(body);
        payload.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let head = format!(
            "POST /upload HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        s.write_all(head.as_bytes()).await.unwrap();
        s.write_all(&payload).await.unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        resp.lines().next().unwrap_or("").to_string()
    }

    #[tokio::test]
    async fn streams_large_file_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("krill-upload-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seen: Arc<Mutex<Vec<ReceivedUpload>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let mut server = UploadServer::default();
        let info = server
            .start(0, dir.clone(), "127.0.0.1".into(), Arc::new(move |f| seen2.lock().unwrap().push(f)))
            .await
            .unwrap();
        assert!(info.qr_svg.starts_with("<svg"));

        // 5 MiB — well past axum's 2 MiB default body limit.
        let big = vec![0xABu8; 5 * 1024 * 1024];
        assert_eq!(post_file(&info.url, "photo.jpg", &big).await, "HTTP/1.1 200 OK");
        assert_eq!(post_file(&info.url, "photo.jpg", b"second").await, "HTTP/1.1 200 OK");
        assert_eq!(post_file(&info.url, "../../evil.txt", b"x").await, "HTTP/1.1 200 OK");

        let got = seen.lock().unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].name, "photo.jpg");
        assert_eq!(got[0].size, big.len() as u64);
        assert_eq!(std::fs::read(&got[0].path).unwrap(), big);
        assert_eq!(got[1].name, "photo (2).jpg");
        assert_eq!(got[2].name, "evil.txt");
        assert!(got[2].path.starts_with(&dir));
        drop(got);

        server.stop().await;
        assert!(server.info().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
