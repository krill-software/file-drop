import "@krill-software/desktop-ui/styles";
import "./styles.css";
import { mountChrome, showBootError, checkForUpdates } from "@krill-software/desktop-ui";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { icons as lucideIcons, createElement as createLucide } from "lucide";

// ---- Types (mirror Rust) -------------------------------------------

interface Identity {
  version: number;
  nodeId: string;
  displayName: string;
  icon: string;
  avatar?: string;
}

interface Contact {
  nodeId: string;
  displayName: string;
  icon: string;
  avatar?: string;
  lastPaired: number;
}

interface NetInfo { nodeId: string; ticket: string }

interface SendStatus {
  phase: "waiting" | "sending" | "done" | "rejected";
  name: string;
  size?: number;
  sent?: number;
  peerId: string;
}

interface RecvStatus {
  phase: "receiving" | "done" | "partial";
  offerId: number;
  name: string;
  size: number;
  got: number;
  path?: string;
  peerId: string;
}

interface Offer {
  offerId: number;
  name: string;
  size: number;
  peerId: string;
}

interface HistoryEntry {
  direction: "sent" | "received";
  name: string;
  size: number;
  at: number;
  path?: string;
  status: "ok" | "rejected" | "partial";
}

interface ReceivedFile {
  name: string;
  size: number;
  modified: number;
  path: string;
}

// Random default display name — adjective-noun-NN. Not globally unique;
// just a sensible default the user can rename inline on their profile.
const ADJ = [
  "swift", "calm", "bright", "quiet", "brave", "kind", "wild", "lucky",
  "sunny", "misty", "lone", "busy", "gentle", "fierce", "clever", "silent",
  "amber", "cosmic", "golden", "silver",
];
const NOUN = [
  "otter", "finch", "river", "comet", "ember", "pine", "fern", "oak",
  "harbor", "willow", "fox", "hawk", "heron", "sparrow", "cedar", "brook",
  "meadow", "aurora", "nebula", "drift",
];
function randomDisplayName(): string {
  const a = ADJ[Math.floor(Math.random() * ADJ.length)];
  const n = NOUN[Math.floor(Math.random() * NOUN.length)];
  const num = Math.floor(10 + Math.random() * 90);
  return `${a}-${n}-${num}`;
}

// ---- Lucide ----------------------------------------------------------

function pascal(name: string): string {
  return name.split("-").map(s => s[0].toUpperCase() + s.slice(1)).join("");
}

function iconSvg(name: string, size = 24): SVGElement {
  const node = (lucideIcons as Record<string, any>)[pascal(name)] ?? lucideIcons.User;
  const el = createLucide(node);
  el.setAttribute("width", String(size));
  el.setAttribute("height", String(size));
  return el;
}

// ---- Avatar processing ---------------------------------------------

const AVATAR_SIZE = 256;

async function fileToAvatarDataUrl(file: File): Promise<string> {
  const url = URL.createObjectURL(file);
  try {
    const img = await loadImage(url);
    const side = Math.min(img.naturalWidth, img.naturalHeight);
    const sx = (img.naturalWidth - side) / 2;
    const sy = (img.naturalHeight - side) / 2;
    const canvas = document.createElement("canvas");
    canvas.width = AVATAR_SIZE;
    canvas.height = AVATAR_SIZE;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("no 2d context");
    ctx.drawImage(img, sx, sy, side, side, 0, 0, AVATAR_SIZE, AVATAR_SIZE);
    return canvas.toDataURL("image/jpeg", 0.88);
  } finally {
    URL.revokeObjectURL(url);
  }
}

function loadImage(src: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error("could not load image"));
    img.src = src;
  });
}

// ---- DOM helpers ----------------------------------------------------

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Record<string, string> = {},
  ...children: (Node | string)[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") node.className = v;
    else node.setAttribute(k, v);
  }
  for (const c of children) {
    node.append(typeof c === "string" ? document.createTextNode(c) : c);
  }
  return node;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function buildAvatar(card: { icon: string; avatar?: string }, size = 48): HTMLElement {
  const visual = el("div", { class: "card-visual" });
  visual.style.width = `${size}px`;
  visual.style.height = `${size}px`;
  if (card.avatar) {
    visual.append(el("img", { src: card.avatar, alt: "" }));
  } else {
    visual.append(iconSvg(card.icon || "user", Math.floor(size * 0.55)));
  }
  return visual;
}

// ---- App state ------------------------------------------------------

let viewportEl: HTMLElement;
let mainContentEl: HTMLElement; // children swapped per view; topbar stays
let auxEl: HTMLElement;
let identity: Identity | null = null;
let myTicket: string | null = null;
let contacts: Contact[] = [];
const presence = new Map<string, boolean>();
const histories = new Map<string, HistoryEntry[]>();
// What's currently showing in the main pane.
let activeView:
  | { kind: "files" }
  | { kind: "connect" }
  | { kind: "session"; peerId: string }
  | { kind: "settings" }
  = { kind: "files" };

let contactFilter = "";
let filesListEl: HTMLElement | null = null;
// Are we connected to the peer currently being viewed?
let liveSession: { peer: Identity } | null = null;

// ---- Profile view (inline edit) ------------------------------------

async function saveIdentity() {
  if (!identity) return;
  await invoke("save_identity", { identity });
  renderAux();
}

interface SettingsView {
  settings: { downloadFolder?: string };
  effectiveFolder: string;
  defaultFolder: string;
}

function renderSettingsView() {
  activeView = { kind: "settings" };
  filesListEl = null;
  if (!identity) return;
  renderAux();

  const root = el("div", { class: "main settings" });

  // --- Profile section ---
  const profileSection = el("section", { class: "settings-section" });
  profileSection.append(el("h2", { class: "section-title" }, "Profile"));

  const avatarBlock = el("div", { class: "profile-avatar-block" });
  const avatarBtn = el("button", {
    class: "profile-avatar",
    type: "button",
    title: "Click to choose a photo",
  });
  const fileInput = el("input", { type: "file", accept: "image/*", class: "setup-file" }) as HTMLInputElement;
  const removeAvatar = el("button", { class: "profile-remove-avatar", type: "button" }, "Remove photo");
  const paint = () => {
    avatarBtn.replaceChildren();
    if (identity!.avatar) {
      avatarBtn.append(el("img", { src: identity!.avatar, alt: "Your avatar" }));
    } else {
      avatarBtn.append(iconSvg("user", 88));
    }
    removeAvatar.hidden = !identity!.avatar;
  };
  paint();
  avatarBtn.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", async () => {
    const f = fileInput.files?.[0];
    if (!f) return;
    try {
      identity!.avatar = await fileToAvatarDataUrl(f);
      paint();
      await saveIdentity();
    } catch (e) { console.warn("avatar failed:", e); }
    fileInput.value = "";
  });
  removeAvatar.addEventListener("click", async () => {
    if (!identity?.avatar) return;
    identity.avatar = undefined;
    paint();
    await saveIdentity();
  });
  avatarBlock.append(avatarBtn, fileInput, removeAvatar);
  profileSection.append(avatarBlock);

  const nameWrap = el("div", { class: "profile-name-wrap" });
  installEditableName(nameWrap);
  profileSection.append(nameWrap);
  root.append(profileSection);

  // --- Downloads section ---
  const downloadsSection = el("section", { class: "settings-section downloads-section" });
  downloadsSection.append(el("h2", { class: "section-title" }, "Download folder"));
  downloadsSection.append(el("p", { class: "hint" },
    "Where incoming files land. Files are saved straight here — no extra subfolder."));

  const folderRow = el("div", { class: "folder-row" });
  const folderPath = el("code", { class: "folder-path" }, "loading…");
  const folderActions = el("div", { class: "folder-actions" });
  const chooseBtn = el("button", { class: "pair-btn", type: "button" }, "Choose…") as HTMLButtonElement;
  const resetBtn = el("button", { class: "pair-btn", type: "button" }, "Reset to default") as HTMLButtonElement;
  folderActions.append(chooseBtn, resetBtn);
  folderRow.append(folderPath, folderActions);
  downloadsSection.append(folderRow);
  const folderHint = el("p", { class: "folder-hint hint" }, "");
  downloadsSection.append(folderHint);

  let current: SettingsView | null = null;
  const repaintFolder = () => {
    if (!current) return;
    folderPath.textContent = current.effectiveFolder;
    const usingDefault = !current.settings.downloadFolder
      || current.settings.downloadFolder === current.defaultFolder;
    resetBtn.disabled = usingDefault;
    folderHint.textContent = usingDefault
      ? `Using your default Downloads folder.`
      : `Override active. Default would be ${current.defaultFolder}.`;
  };

  void (async () => {
    try {
      current = await invoke<SettingsView>("load_settings");
      repaintFolder();
    } catch (e) { console.warn("load_settings failed:", e); }
  })();

  chooseBtn.addEventListener("click", async () => {
    try {
      const picked = await openDialog({
        directory: true,
        multiple: false,
        defaultPath: current?.effectiveFolder,
      });
      if (typeof picked !== "string" || !picked) return;
      const next: SettingsView = await invoke("save_settings", {
        settings: { downloadFolder: picked },
      });
      current = next;
      repaintFolder();
      void refreshFilesList();
    } catch (e) { console.warn("choose folder failed:", e); }
  });

  resetBtn.addEventListener("click", async () => {
    try {
      const next: SettingsView = await invoke("save_settings", {
        settings: {},
      });
      current = next;
      repaintFolder();
      void refreshFilesList();
    } catch (e) { console.warn("reset folder failed:", e); }
  });

  root.append(downloadsSection);

  mainContentEl.replaceChildren(root);
  // Spin iroh up in the background so the code is ready in Connect.
  void ensureNetworkSilent();
}

function installEditableName(wrap: HTMLElement) {
  if (!identity) return;
  const display = el("h1", {
    class: "profile-name",
    title: "Click to edit",
  }, identity.displayName);
  display.addEventListener("click", () => {
    const input = el("input", {
      class: "profile-name-input",
      type: "text",
      maxlength: "40",
      value: identity!.displayName,
    }) as HTMLInputElement;
    display.replaceWith(input);
    input.focus();
    input.select();
    let committed = false;
    const commit = async (save: boolean) => {
      if (committed) return;
      committed = true;
      const v = input.value.trim();
      if (save && v && v !== identity!.displayName) {
        identity!.displayName = v;
        await saveIdentity();
      }
      wrap.replaceChildren();
      installEditableName(wrap);
    };
    input.addEventListener("blur", () => void commit(true));
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") { e.preventDefault(); void commit(true); }
      if (e.key === "Escape") { e.preventDefault(); void commit(false); }
    });
  });
  wrap.append(display);
}

// ---- Aux pane (contacts list) --------------------------------------

function renderAux() {
  auxEl.replaceChildren();

  // Hamburger sits at the very top of the sidebar.
  auxEl.append(buildAuxTopbar());

  // Settings nav row (top) — gear icon + label, matches Connect / Files.
  const settingsRow = el("button", {
    class: "aux-nav",
    type: "button",
    title: "Settings",
  });
  if (activeView.kind === "settings") settingsRow.setAttribute("data-selected", "true");
  const settingsIcon = el("div", { class: "aux-nav-icon" });
  settingsIcon.append(iconSvg("settings", 18));
  settingsRow.append(settingsIcon);
  const settingsText = el("div", { class: "aux-nav-text" });
  settingsText.append(el("div", { class: "aux-nav-name" }, "Settings"));
  settingsText.append(el("div", { class: "aux-nav-sub" }, "you and your folders"));
  settingsRow.append(settingsText);
  settingsRow.addEventListener("click", () => renderSettingsView());
  auxEl.append(settingsRow);

  // Connect row
  const connectRow = el("button", { class: "aux-nav", type: "button", title: "Connect with someone" });
  if (activeView.kind === "connect") connectRow.setAttribute("data-selected", "true");
  const connectIconWrap = el("div", { class: "aux-nav-icon" });
  connectIconWrap.append(iconSvg("link", 18));
  connectRow.append(connectIconWrap);
  const connectText = el("div", { class: "aux-nav-text" });
  connectText.append(el("div", { class: "aux-nav-name" }, "Connect"));
  connectText.append(el("div", { class: "aux-nav-sub" }, "pair with a friend"));
  connectRow.append(connectText);
  connectRow.addEventListener("click", () => renderConnectView());
  auxEl.append(connectRow);

  // Files row
  const filesRow = el("button", { class: "aux-nav", type: "button", title: "Received files" });
  if (activeView.kind === "files") filesRow.setAttribute("data-selected", "true");
  const filesIconWrap = el("div", { class: "aux-nav-icon" });
  filesIconWrap.append(iconSvg("inbox", 18));
  filesRow.append(filesIconWrap);
  const filesText = el("div", { class: "aux-nav-text" });
  filesText.append(el("div", { class: "aux-nav-name" }, "Files"));
  filesText.append(el("div", { class: "aux-nav-sub" }, "what people sent you"));
  filesRow.append(filesText);
  filesRow.addEventListener("click", () => renderFilesView());
  auxEl.append(filesRow);

  // Contacts section
  const header = el("div", { class: "aux-header" });
  header.append(el("span", { class: "aux-title" }, "Contacts"));
  auxEl.append(header);

  // Filter input — above the list.
  const sorted = [...contacts].sort((a, b) => b.lastPaired - a.lastPaired);
  const filterWrap = el("div", { class: "aux-filter" });
  filterWrap.append(iconSvg("search", 14));
  const filterInput = el("input", {
    class: "aux-filter-input",
    type: "text",
    placeholder: "Filter contacts",
    value: contactFilter,
  }) as HTMLInputElement;
  filterInput.addEventListener("input", () => {
    contactFilter = filterInput.value;
    repaintContactList(sorted);
  });
  filterWrap.append(filterInput);
  auxEl.append(filterWrap);

  const list = el("div", { class: "aux-list" });
  auxEl.append(list);
  repaintContactList(sorted);

  // Version footer pinned to the bottom of the sidebar.
  auxEl.append(el("div", { class: "aux-version" }, `v${__APP_VERSION__}`));
}

function repaintContactList(sorted: Contact[]) {
  const listEl = auxEl.querySelector(".aux-list") as HTMLElement | null;
  if (!listEl) return;
  listEl.replaceChildren();
  const f = contactFilter
    ? sorted.filter((c) => c.displayName.toLowerCase().includes(contactFilter.toLowerCase()))
    : sorted;
  if (sorted.length === 0) {
    listEl.append(el("p", { class: "aux-empty" },
      "No contacts yet. Connect once via a code and they'll appear here."));
  } else if (f.length === 0) {
    listEl.append(el("p", { class: "aux-empty" }, `No contacts match "${contactFilter}".`));
  } else {
    for (const c of f) listEl.append(buildContactTile(c));
  }
}

function buildContactTile(c: Contact): HTMLElement {
  const tile = el("button", {
    class: "contact-tile",
    type: "button",
    "data-node-id": c.nodeId,
  });
  if (activeView.kind === "session" && activeView.peerId === c.nodeId) {
    tile.setAttribute("data-selected", "true");
  }
  const online = presence.get(c.nodeId) === true;
  tile.setAttribute("data-online", online ? "true" : "false");
  tile.append(buildAvatar(c, 36));
  const text = el("div", { class: "contact-text" });
  text.append(el("div", { class: "contact-name" }, c.displayName));
  text.append(el("div", { class: "contact-meta" }, online ? "online" : "offline"));
  const dot = el("span", { class: "presence-dot", title: online ? "online" : "offline" });
  tile.append(text, dot);
  tile.addEventListener("click", () => openContactView(c));
  return tile;
}

function refreshOneContactPresence(nodeId: string) {
  const tile = auxEl.querySelector(`[data-node-id="${CSS.escape(nodeId)}"]`) as HTMLElement | null;
  if (!tile) return;
  const online = presence.get(nodeId) === true;
  tile.setAttribute("data-online", online ? "true" : "false");
  const meta = tile.querySelector(".contact-meta");
  if (meta) meta.textContent = online ? "online" : "offline";
}

// ---- Home view (idle) ---------------------------------------------

function renderFilesView() {
  activeView = { kind: "files" };
  renderAux();
  if (!identity) return;
  const root = el("div", { class: "main files" });

  const filesSection = el("section", { class: "files-section" });
  filesSection.append(el("h2", { class: "section-title" }, "Received files"));
  const listEl = el("div", { class: "files-list" });
  filesSection.append(listEl);
  filesListEl = listEl;
  void refreshFilesList();

  root.append(filesSection);
  mainContentEl.replaceChildren(root);

  if (!myTicket) void ensureNetworkSilent();
}

function renderConnectView() {
  activeView = { kind: "connect" };
  filesListEl = null;
  renderAux();
  if (!identity) return;
  const root = el("div", { class: "main connect-view" });

  // Your code
  const yours = el("section", { class: "your-code" });
  yours.append(el("h2", { class: "section-title" }, "Your code"));
  yours.append(el("p", { class: "hint" },
    "Share this with a friend so they can connect to you."));
  const codeBtn = el("button", {
    class: "profile-copy-code",
    type: "button",
    ...(myTicket ? {} : { disabled: "" }),
  }) as HTMLButtonElement;
  const repaintCopyBtn = () => {
    codeBtn.replaceChildren();
    codeBtn.append(iconSvg("link", 14));
    codeBtn.append(document.createTextNode(myTicket ? "Copy your code" : "starting network…"));
  };
  repaintCopyBtn();
  codeBtn.addEventListener("click", async () => {
    if (!myTicket) return;
    try {
      await navigator.clipboard.writeText(myTicket);
      codeBtn.replaceChildren();
      codeBtn.append(iconSvg("check", 14));
      codeBtn.append(document.createTextNode("Copied"));
      setTimeout(repaintCopyBtn, 1200);
    } catch { /* ignore */ }
  });
  yours.append(codeBtn);

  // Connect with
  const connect = el("section", { class: "connect" });
  connect.append(el("h2", { class: "section-title" }, "Connect with"));
  const connectBox = el("div", { class: "ticket-box" });
  const pasteInput = el("input", {
    class: "ticket-input",
    placeholder: "paste a friend's code here",
  }) as HTMLInputElement;
  const dialBtn = el("button", { class: "pair-btn primary", type: "button" }, "Connect") as HTMLButtonElement;
  connectBox.append(pasteInput, dialBtn);
  const connectStatus = el("div", { class: "pair-status" });
  connect.append(connectBox, connectStatus);

  const dial = async () => {
    const t = pasteInput.value.trim();
    if (!t) return;
    dialBtn.disabled = true;
    pasteInput.disabled = true;
    connectStatus.textContent = "Connecting…";
    connectStatus.dataset.kind = "info";
    try {
      await invoke("connect_to_ticket", { ticket: t });
    } catch (e: any) {
      connectStatus.textContent = `Couldn't connect: ${e}`;
      connectStatus.dataset.kind = "err";
      dialBtn.disabled = false;
      pasteInput.disabled = false;
    }
  };
  dialBtn.addEventListener("click", () => void dial());
  pasteInput.addEventListener("keydown", (e) => { if (e.key === "Enter") void dial(); });

  // How-to
  const howto = el("section", { class: "howto" });
  howto.append(el("h2", { class: "section-title" }, "How it works"));
  const list = el("ol", { class: "howto-list" });
  const steps = [
    "Tap Copy your code above.",
    "Send it to the person you want to connect with (Signal, iMessage, email — anywhere).",
    "Have them paste their code back to you, paste it above, then hit Connect.",
    "Once paired they'll appear in your Contacts and you can drop files to each other.",
  ];
  for (const s of steps) list.append(el("li", {}, s));
  howto.append(list);

  root.append(yours, connect, howto);
  mainContentEl.replaceChildren(root);

  if (!myTicket) {
    void (async () => {
      await ensureNetworkSilent();
      repaintCopyBtn();
      codeBtn.disabled = !myTicket;
    })();
  }
}

async function refreshFilesList() {
  if (!filesListEl) return;
  try {
    const files = await invoke<ReceivedFile[]>("list_received_files");
    if (!filesListEl) return;
    filesListEl.replaceChildren();
    if (files.length === 0) {
      filesListEl.append(el("p", { class: "hint" },
        "Nothing yet. Files people send you will land here."));
      return;
    }
    for (const f of files) filesListEl.append(buildFileRow(f));
  } catch (e) {
    console.error("list_received_files failed:", e);
  }
}

function buildFileRow(f: ReceivedFile): HTMLElement {
  const row = el("div", { class: "file-row", title: f.path });

  const open = el("button", { class: "file-open", type: "button" });
  open.append(iconSvg("file", 18));
  const info = el("div", { class: "file-info" });
  info.append(el("div", { class: "file-name" }, f.name));
  info.append(el("div", { class: "file-meta" },
    `${formatBytes(f.size)} · ${new Date(f.modified).toLocaleString()}`));
  open.append(info);
  open.addEventListener("click", async () => {
    try { await invoke("open_file", { path: f.path }); }
    catch (e) { console.warn("open_file failed:", e); }
  });

  const copy = el("button", {
    class: "file-copy",
    type: "button",
    title: "Copy file (paste into a folder)",
  });
  copy.append(iconSvg("copy", 16));
  copy.addEventListener("click", async (e) => {
    e.stopPropagation();
    copy.replaceChildren();
    copy.append(iconSvg("loader", 16));
    try {
      await invoke("copy_file_to_clipboard", { path: f.path });
      copy.replaceChildren();
      copy.append(iconSvg("check", 16));
      copy.dataset.state = "ok";
      setTimeout(() => {
        copy.replaceChildren();
        copy.append(iconSvg("copy", 16));
        delete copy.dataset.state;
      }, 1200);
    } catch (err) {
      copy.replaceChildren();
      copy.append(iconSvg("triangle-alert", 16));
      copy.dataset.state = "err";
      copy.title = `Couldn't copy: ${err}`;
      setTimeout(() => {
        copy.replaceChildren();
        copy.append(iconSvg("copy", 16));
        copy.title = "Copy file (paste into a folder)";
        delete copy.dataset.state;
      }, 2400);
    }
  });

  row.append(open, copy);
  return row;
}

async function ensureNetworkSilent() {
  if (!identity || myTicket) return;
  try {
    const info = await invoke<NetInfo>("start_network", { identity });
    myTicket = info.ticket;
    if (identity.nodeId !== info.nodeId) identity.nodeId = info.nodeId;
  } catch (e) {
    console.error(e);
  }
}

// ---- Session / contact-detail view --------------------------------

let sessionStatusEl: HTMLElement | null = null;
let sessionOffersEl: HTMLElement | null = null;
let sessionDropEl: HTMLElement | null = null;
let sessionHistoryEl: HTMLElement | null = null;

function openContactView(c: Contact) {
  const isLive = liveSession?.peer.nodeId === c.nodeId;
  const peer: Identity = isLive ? liveSession!.peer : {
    version: 1,
    nodeId: c.nodeId,
    displayName: c.displayName,
    icon: c.icon,
    avatar: c.avatar,
  };
  renderSessionView(peer, { live: isLive });
  if (!isLive) {
    // Try to dial; on success the session-started event will re-render.
    void invoke("dial_contact", { nodeId: c.nodeId }).catch((err) => {
      console.warn("dial failed:", err);
      const status = sessionStatusEl;
      if (status) {
        status.replaceChildren(el("div", { class: "status-line waiting" },
          `${c.displayName} didn't answer. They may be offline.`));
      }
    });
  }
}

function renderSessionView(peer: Identity, opts: { live: boolean }) {
  activeView = { kind: "session", peerId: peer.nodeId };
  filesListEl = null;
  renderAux();

  const root = el("div", { class: "main session" });

  // Connected-with header
  const header = el("section", { class: "session-header" });
  header.append(el("h2", { class: "section-title" }, opts.live ? "Connected with" : "Contact"));
  const peerTile = el("div", { class: "card-tile peer" });
  peerTile.append(buildAvatar(peer, 56));
  const peerText = el("div", { class: "card-text" });
  peerText.append(el("div", { class: "card-name big" }, peer.displayName));
  const sub = el("div", { class: "card-sub" });
  if (opts.live) sub.textContent = "online · connected";
  else sub.textContent = presence.get(peer.nodeId) ? "online · dialing…" : "offline";
  peerText.append(sub);
  peerTile.append(peerText);

  if (opts.live) {
    const actions = el("div", { class: "peer-actions" });
    const disconnect = el("button", { class: "pair-btn", type: "button" }, "Disconnect");
    disconnect.addEventListener("click", async () => {
      await invoke("disconnect_session").catch(() => {});
    });
    actions.append(disconnect);
    peerTile.append(actions);
  }
  header.append(peerTile);

  // Drop zone
  const drop = el("section", { class: "drop-section" });
  const zone = el("div", { class: "drop-zone" });
  if (opts.live) {
    zone.append(iconSvg("upload", 32));
    zone.append(el("div", { class: "drop-text" }, "Drop a file anywhere on the window"));
    zone.append(el("div", { class: "drop-sub" }, `to send to ${peer.displayName}`));
  } else {
    zone.setAttribute("data-disabled", "true");
    zone.append(iconSvg("upload", 32));
    zone.append(el("div", { class: "drop-text" }, "Not connected"));
    zone.append(el("div", { class: "drop-sub" }, `Reaching ${peer.displayName}…`));
  }
  drop.append(zone);

  const status = el("div", { class: "session-status" });
  const offers = el("div", { class: "offers" });

  // History
  const history = el("section", { class: "history" });
  history.append(el("h2", { class: "section-title" }, "History"));
  const historyList = el("div", { class: "history-list" });
  history.append(historyList);

  root.append(header, drop, offers, status, history);
  mainContentEl.replaceChildren(root);

  sessionStatusEl = status;
  sessionOffersEl = offers;
  sessionDropEl = zone;
  sessionHistoryEl = historyList;

  renderHistory(peer.nodeId);
}

function setDropActive(active: boolean) {
  if (sessionDropEl && sessionDropEl.getAttribute("data-disabled") !== "true") {
    sessionDropEl.dataset.active = active ? "true" : "false";
  }
}

// ---- Status renderers ---------------------------------------------

function renderSendStatus(s: SendStatus) {
  // Only render in the matching peer view.
  if (activeView.kind !== "session" || activeView.peerId !== s.peerId) {
    appendHistoryFromSend(s);
    return;
  }
  if (!sessionStatusEl) return;
  sessionStatusEl.replaceChildren();
  let line: string;
  let pct: number | null = null;
  switch (s.phase) {
    case "waiting":
      line = `Waiting for them to accept ${s.name}…`; break;
    case "sending":
      pct = s.size ? Math.min(100, Math.floor((s.sent ?? 0) * 100 / s.size)) : null;
      line = pct != null ? `Sending ${s.name} — ${pct}%` : `Sending ${s.name}…`;
      break;
    case "done":
      line = `Sent ${s.name}.`; break;
    case "rejected":
      line = `They declined ${s.name}.`; break;
  }
  sessionStatusEl.append(el("div", { class: `status-line ${s.phase}` }, line));
  if (pct != null) {
    const bar = el("div", { class: "progress" });
    const fill = el("div", { class: "progress-fill" });
    fill.style.width = `${pct}%`;
    bar.append(fill);
    sessionStatusEl.append(bar);
  }
  appendHistoryFromSend(s);
}

function appendHistoryFromSend(s: SendStatus) {
  if (s.phase !== "done" && s.phase !== "rejected") return;
  pushHistory(s.peerId, {
    direction: "sent",
    name: s.name,
    size: s.size ?? 0,
    at: Date.now(),
    status: s.phase === "done" ? "ok" : "rejected",
  });
}

function renderOffer(offer: Offer) {
  if (activeView.kind !== "session" || activeView.peerId !== offer.peerId) {
    // Auto-accept / decline UI is per-peer; if not on that peer's view,
    // we could surface a notification. For now, the user just needs to
    // click the contact in the aux pane to see the prompt.
    // (The offer is still pending — we don't time it out.)
    return;
  }
  if (!sessionOffersEl) return;
  const row = el("div", { class: "offer", "data-offer-id": String(offer.offerId) });
  const info = el("div", { class: "offer-info" });
  info.append(el("div", { class: "offer-name" }, offer.name));
  info.append(el("div", { class: "offer-meta" }, formatBytes(offer.size)));
  const accept = el("button", { class: "pair-btn primary", type: "button" }, "Accept");
  const decline = el("button", { class: "pair-btn", type: "button" }, "Decline");
  accept.addEventListener("click", async () => {
    accept.disabled = true; decline.disabled = true;
    try { await invoke("respond_to_offer", { offerId: offer.offerId, accept: true }); }
    catch (e) { console.warn(e); }
  });
  decline.addEventListener("click", async () => {
    try { await invoke("respond_to_offer", { offerId: offer.offerId, accept: false }); }
    catch (e) { console.warn(e); }
    row.remove();
  });
  row.append(info, accept, decline);
  sessionOffersEl.append(row);
}

function renderRecvStatus(s: RecvStatus) {
  if (activeView.kind === "session" && activeView.peerId === s.peerId && sessionOffersEl) {
    const row = sessionOffersEl.querySelector(
      `[data-offer-id="${s.offerId}"]`,
    ) as HTMLElement | null;
    if (row) {
      row.replaceChildren();
      const info = el("div", { class: "offer-info" });
      info.append(el("div", { class: "offer-name" }, s.name));
      if (s.phase === "receiving") {
        const pct = s.size ? Math.min(100, Math.floor(s.got * 100 / s.size)) : 0;
        info.append(el("div", { class: "offer-meta" }, `Receiving — ${pct}%`));
        const bar = el("div", { class: "progress" });
        const fill = el("div", { class: "progress-fill" });
        fill.style.width = `${pct}%`;
        bar.append(fill);
        row.append(info, bar);
      } else if (s.phase === "done") {
        info.append(el("div", { class: "offer-meta ok" },
          `Saved · ${s.path ?? "~/Downloads/krill-file-drop/"}`));
        row.append(info);
      } else {
        info.append(el("div", { class: "offer-meta err" },
          `Incomplete (${formatBytes(s.got)} of ${formatBytes(s.size)})`));
        row.append(info);
      }
    }
  }
  if (s.phase === "done" || s.phase === "partial") {
    pushHistory(s.peerId, {
      direction: "received",
      name: s.name,
      size: s.size,
      at: Date.now(),
      path: s.path,
      status: s.phase === "done" ? "ok" : "partial",
    });
    void refreshFilesList();
  }
}

// ---- History --------------------------------------------------------

function pushHistory(peerId: string, entry: HistoryEntry) {
  const arr = histories.get(peerId) ?? [];
  arr.unshift(entry);
  if (arr.length > 100) arr.length = 100;
  histories.set(peerId, arr);
  if (activeView.kind === "session" && activeView.peerId === peerId) {
    renderHistory(peerId);
  }
}

function renderHistory(peerId: string) {
  if (!sessionHistoryEl) return;
  sessionHistoryEl.replaceChildren();
  const arr = histories.get(peerId) ?? [];
  if (arr.length === 0) {
    sessionHistoryEl.append(el("p", { class: "hint" }, "No transfers yet."));
    return;
  }
  for (const e of arr) {
    const row = el("div", { class: `history-row ${e.status}` });
    const icon = iconSvg(e.direction === "sent" ? "arrow-up" : "arrow-down", 14);
    const info = el("div", { class: "history-info" });
    info.append(el("div", { class: "history-name" }, e.name));
    const meta: string[] = [formatBytes(e.size)];
    if (e.status === "rejected") meta.push("declined");
    else if (e.status === "partial") meta.push("incomplete");
    meta.push(new Date(e.at).toLocaleTimeString());
    info.append(el("div", { class: "history-meta" }, meta.join(" · ")));
    row.append(icon, info);
    sessionHistoryEl.append(row);
  }
}

// ---- Drag/drop wiring ----------------------------------------------

async function installFileDrop() {
  const wv = getCurrentWebview();
  await wv.onDragDropEvent(async (e) => {
    if (e.payload.type === "over") {
      setDropActive(true);
    } else if (e.payload.type === "leave") {
      setDropActive(false);
    } else if (e.payload.type === "drop") {
      setDropActive(false);
      if (!liveSession) return;
      if (activeView.kind !== "session" || activeView.peerId !== liveSession.peer.nodeId) return;
      const path = e.payload.paths[0];
      if (!path) return;
      try { await invoke("send_file", { path }); }
      catch (err) { console.error("send_file failed:", err); }
    }
  });
}

// ---- Main topbar (window controls + hamburger) ----------------------

function buildMainTopbar(): HTMLElement {
  const bar = el("div", { class: "main-topbar", "data-tauri-drag-region": "true" });

  const min = el("button", { class: "main-topbar-btn", type: "button", title: "Minimize" });
  min.append(iconSvg("minus", 16));
  min.addEventListener("click", () => { void getCurrentWindow().minimize(); });

  const max = el("button", { class: "main-topbar-btn", type: "button", title: "Maximize" });
  max.append(iconSvg("square", 14));
  max.addEventListener("click", () => { void getCurrentWindow().toggleMaximize(); });

  const close = el("button", {
    class: "main-topbar-btn",
    type: "button",
    title: "Close",
    "data-kind": "close",
  });
  close.append(iconSvg("x", 16));
  close.addEventListener("click", () => { void getCurrentWindow().close(); });

  bar.append(min, max, close);
  return bar;
}

function buildAuxTopbar(): HTMLElement {
  const bar = el("div", { class: "aux-topbar", "data-tauri-drag-region": "true" });
  const hamburger = el("button", {
    class: "main-topbar-btn",
    type: "button",
    title: "Menu",
  });
  hamburger.append(iconSvg("menu", 16));
  hamburger.addEventListener("click", (e) => {
    e.stopPropagation();
    toggleHamburgerMenu(bar);
  });
  bar.append(hamburger);
  return bar;
}

function toggleHamburgerMenu(anchor: HTMLElement) {
  const existing = document.querySelector(".menu-popover");
  if (existing) { existing.remove(); return; }

  const pop = el("div", { class: "menu-popover" });
  const items: Array<{ label: string; action: () => void } | { sep: true }> = [
    { label: "Check for updates…", action: () => void checkForUpdates("File Drop") },
    { sep: true },
    { label: "Quit", action: () => void getCurrentWindow().close() },
  ];
  for (const it of items) {
    if ("sep" in it) {
      pop.append(el("div", { class: "menu-popover-sep" }));
    } else {
      const btn = el("button", { class: "menu-popover-item", type: "button" }, it.label);
      btn.addEventListener("click", () => { pop.remove(); it.action(); });
      pop.append(btn);
    }
  }
  // Position relative to viewport — anchor is the topbar.
  anchor.parentElement?.append(pop);
  // Dismiss on outside click.
  setTimeout(() => {
    const handler = (ev: MouseEvent) => {
      if (!pop.contains(ev.target as Node)) {
        pop.remove();
        document.removeEventListener("click", handler);
      }
    };
    document.addEventListener("click", handler);
  }, 0);
}

// ---- Boot -----------------------------------------------------------

async function boot() {
  const chrome = mountChrome({
    productName: "File Drop",
    actions: {},
    showStatusLine: false,
    showAuxPane: true,
    updater: true,
  });
  viewportEl = chrome.viewport;
  auxEl = chrome.aux!;
  auxEl.classList.add("contacts-aux");

  // Shell-app layout: the main pane gets its own topbar (drag region +
  // hamburger + window controls), and a separate scrollable content area
  // that each renderXView swaps. The desktop-ui titlebar + status line
  // are hidden via styles.css for this app.
  const topbar = buildMainTopbar();
  mainContentEl = el("div", { class: "main-content" });
  viewportEl.replaceChildren(topbar, mainContentEl);

  identity = await invoke<Identity | null>("load_identity");

  if (!identity || !identity.displayName) {
    identity = {
      version: 1,
      nodeId: "",
      displayName: randomDisplayName(),
      icon: "user",
      avatar: undefined,
    };
    await invoke("save_identity", { identity });
  }

  await listen<Identity>("session-started", (e) => {
    liveSession = { peer: e.payload };
    // If the user wasn't viewing this peer, jump them there.
    renderSessionView(e.payload, { live: true });
  });
  await listen<{ peerId: string }>("session-ended", (e) => {
    if (liveSession && liveSession.peer.nodeId === e.payload.peerId) {
      liveSession = null;
    }
    if (activeView.kind === "session" && activeView.peerId === e.payload.peerId) {
      const c = contacts.find((c) => c.nodeId === e.payload.peerId);
      if (c) renderSessionView(toIdentity(c), { live: false });
      else renderFilesView();
    }
  });
  await listen<SendStatus>("send-status", (e) => renderSendStatus(e.payload));
  await listen<Offer>("file-offered", (e) => renderOffer(e.payload));
  await listen<RecvStatus>("recv-status", (e) => renderRecvStatus(e.payload));
  await listen<Contact[]>("contacts-updated", (e) => {
    contacts = e.payload;
    renderAux();
  });
  await listen<{ nodeId: string; online: boolean }>("contact-presence", (e) => {
    presence.set(e.payload.nodeId, e.payload.online);
    refreshOneContactPresence(e.payload.nodeId);
    // If we're viewing this peer's detail and they came online while we
    // were waiting offline, leave the view alone — the session-started
    // event will swap it. For simple offline-meta refresh:
    if (activeView.kind === "session" && activeView.peerId === e.payload.nodeId && !liveSession) {
      const sub = viewportEl.querySelector(".session-header .card-sub");
      if (sub) sub.textContent = e.payload.online ? "online · click to connect" : "offline";
    }
  });

  contacts = await invoke<Contact[]>("list_contacts").catch(() => []);
  await hydrateHistories();
  renderAux();
  await installFileDrop();
  renderFilesView();
}

interface BackendTransfer {
  id: string;
  direction: "received" | "sent";
  peerId: string;
  peerName: string;
  name: string;
  size: number;
  at: number;
  path?: string;
  status: "ok" | "partial" | "rejected";
}

async function hydrateHistories() {
  try {
    const all = await invoke<BackendTransfer[]>("list_history");
    histories.clear();
    // Backend stores oldest-first; the per-peer Map wants newest-first
    // so iterate in reverse and unshift each into its peer's array.
    for (const t of all) {
      const arr = histories.get(t.peerId) ?? [];
      arr.push({
        direction: t.direction,
        name: t.name,
        size: t.size,
        at: t.at,
        path: t.path,
        status: t.status,
      });
      histories.set(t.peerId, arr);
    }
    // Now reverse each peer's array so newest is first.
    for (const [k, arr] of histories) {
      arr.reverse();
      histories.set(k, arr);
    }
  } catch (e) {
    console.warn("hydrateHistories failed:", e);
  }
}

function toIdentity(c: Contact): Identity {
  return { version: 1, nodeId: c.nodeId, displayName: c.displayName, icon: c.icon, avatar: c.avatar };
}

boot().catch((e) => {
  console.error("boot failed:", e);
  showBootError(e);
});
