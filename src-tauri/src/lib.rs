mod network;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tokio::sync::Mutex;

use krill_desktop_core::{state as kstate, updater::BuilderExt};

const SLUG: &str = "krill-file-drop";

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct Identity {
    version: u32,
    #[serde(rename = "nodeId", default)]
    node_id: String,
    #[serde(rename = "displayName")]
    display_name: String,
    icon: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    avatar: Option<String>,
}

impl Identity {
    fn to_card(&self) -> network::Card {
        network::Card {
            version: self.version.max(1),
            node_id: self.node_id.clone(),
            display_name: self.display_name.clone(),
            icon: self.icon.clone(),
            avatar: self.avatar.clone(),
        }
    }
}

#[derive(Default)]
struct AppNetState {
    net: Mutex<Option<network::Network>>,
}

#[tauri::command]
fn load_identity() -> Option<Identity> {
    kstate::load(SLUG, "identity.json")
}

#[tauri::command]
async fn save_identity(
    identity: Identity,
    state: State<'_, Arc<AppNetState>>,
) -> Result<(), String> {
    kstate::save(SLUG, "identity.json", &identity)?;
    if let Some(net) = state.net.lock().await.as_ref() {
        net.update_card(identity.to_card()).await;
    }
    Ok(())
}

#[tauri::command]
fn default_display_name() -> String {
    std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("USERNAME").ok())
        .or_else(|| hostname())
        .unwrap_or_else(|| "me".to_string())
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[derive(Debug, Serialize)]
struct NetInfo {
    #[serde(rename = "nodeId")]
    node_id: String,
    ticket: String,
}

#[tauri::command]
async fn start_network(
    identity: Identity,
    app: AppHandle,
    state: State<'_, Arc<AppNetState>>,
) -> Result<NetInfo, String> {
    let mut guard = state.net.lock().await;
    if let Some(net) = guard.as_ref() {
        let ticket = net.ticket().await.map_err(|e| e.to_string())?;
        return Ok(NetInfo {
            node_id: net.node_id(),
            ticket,
        });
    }
    let dir = network::state_dir();
    let sk = network::load_or_create_secret_key(&dir).map_err(|e| e.to_string())?;
    let net = network::Network::start(sk, identity.to_card(), app)
        .await
        .map_err(|e| e.to_string())?;
    let ticket = net.ticket().await.map_err(|e| e.to_string())?;
    let info = NetInfo {
        node_id: net.node_id(),
        ticket,
    };

    // Persist the freshly-known NodeID back into identity.json so it's
    // visible on next launch even before networking starts.
    if identity.node_id != info.node_id {
        let updated = Identity {
            node_id: info.node_id.clone(),
            ..identity
        };
        let _ = kstate::save(SLUG, "identity.json", &updated);
    }

    *guard = Some(net);
    Ok(info)
}

async fn current_net(state: &State<'_, Arc<AppNetState>>) -> Result<network::Network, String> {
    let guard = state.net.lock().await;
    guard.as_ref().cloned().ok_or_else(|| "network not started".to_string())
}

#[tauri::command]
async fn connect_to_ticket(
    ticket: String,
    app: AppHandle,
    state: State<'_, Arc<AppNetState>>,
) -> Result<(), String> {
    let net = current_net(&state).await?;
    net.connect_to_ticket(&ticket, app)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn send_file(
    path: String,
    app: AppHandle,
    state: State<'_, Arc<AppNetState>>,
) -> Result<(), String> {
    let net = current_net(&state).await?;
    net.send_file(std::path::Path::new(&path), app)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn respond_to_offer(
    offer_id: u64,
    accept: bool,
    state: State<'_, Arc<AppNetState>>,
) -> Result<(), String> {
    let net = current_net(&state).await?;
    net.respond_to_offer(offer_id, accept)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn disconnect_session(state: State<'_, Arc<AppNetState>>) -> Result<(), String> {
    let net = current_net(&state).await?;
    net.disconnect().await;
    Ok(())
}

#[tauri::command]
async fn list_contacts(state: State<'_, Arc<AppNetState>>) -> Result<Vec<network::Contact>, String> {
    let net = current_net(&state).await?;
    Ok(net.list_contacts().await)
}

#[tauri::command]
fn list_received_files() -> Vec<network::ReceivedFile> {
    network::list_received_files()
}

#[tauri::command]
fn open_file(path: String) -> Result<(), String> {
    network::open_path(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn copy_file_to_clipboard(path: String) -> Result<(), String> {
    network::copy_file_to_clipboard(&path).map_err(|e| e.to_string())
}

#[tauri::command]
async fn dial_contact(
    node_id: String,
    app: AppHandle,
    state: State<'_, Arc<AppNetState>>,
) -> Result<(), String> {
    let net = current_net(&state).await?;
    net.dial_contact(&node_id, app)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct AppState {
    window: Option<kstate::WindowGeometry>,
}

#[tauri::command]
fn load_state() -> Option<AppState> {
    kstate::load(SLUG, "state.json")
}

#[tauri::command]
fn save_state(state: AppState) -> Result<(), String> {
    kstate::save(SLUG, "state.json", &state)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let net_state = Arc::new(AppNetState::default());
    tauri::Builder::default()
        .manage(net_state)
        .with_updater()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            load_identity,
            save_identity,
            default_display_name,
            start_network,
            connect_to_ticket,
            send_file,
            respond_to_offer,
            disconnect_session,
            list_contacts,
            dial_contact,
            list_received_files,
            open_file,
            copy_file_to_clipboard,
            load_state,
            save_state,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
