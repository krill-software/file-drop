# file-drop — SPEC

## In one sentence

**Pair with a friend once via a ticket or QR; after that, click their card to drop a file.**

WAN by default. End-to-end encrypted. No accounts, no krill-operated infrastructure.

## Identity

| Where                | Value                                          |
|----------------------|------------------------------------------------|
| Slug                 | `file-drop`                                    |
| Binary               | `krill-file-drop`                              |
| Cargo package        | `krill-file-drop`                              |
| Cargo lib            | `krill_file_drop_lib`                          |
| `package.json` name  | `krill-file-drop`                              |
| Bundle identifier    | `software.krill.file-drop`                     |
| productName          | `File Drop`                                    |
| State dir            | `$XDG_STATE_HOME/krill-file-drop/`             |
| GitHub repo          | `krill-software/file-drop`                     |
| Lucide icon          | `send`                                         |

## Does this fit krill?

It's the named exception in the umbrella, and worth being honest about why.

**Pulls in the krill direction.** One job, sayable in a sentence. No editing, no library, no settings panel. Calm metaphor (AirDrop) Win/Mac switchers already understand.

**Pulls against.** It's networked, which drags in identity, encryption, and NAT. A LAN-only version solves no real problem (your friend in Tokyo isn't on your Wi-Fi). So we go WAN, and the only way to do that without operating infrastructure is to ride a stack that already exists with public servers — [Iroh](https://www.iroh.computer). This means accepting one new dependency on community infrastructure (n0's free public relays) for v1. That dependency is the krill-uncomfortable part.

**The verdict.** It fits if and only if (a) we stay on Iroh's public infrastructure (no krill-operated servers) and (b) we refuse to expand scope past "drop a file." If we add chat, history, folder sync, or store-and-forward, we've left krill.

## Architecture

### Two-phase flow

```
First pair (once per peer)        Subsequent transfer (every time after)
──────────────────────────        ─────────────────────────────────────
Both apps open                    Sender clicks saved contact tile
Receiver shares ticket or QR      Iroh dials over relay
Sender pastes / scans             Receiver gets "Filip wants to send — accept?"
Apps verify, exchange cards       Sender drags file
        │                                       │
        └────────► Drag files ◄─────────────────┘
                          │
                          ▼
              Encrypted P2P session is live
              Files transfer one after another
              Either side closes → session ends
```

### Transport: Iroh

[Iroh](https://www.iroh.computer) provides three things in one stack: persistent NodeID-as-identity (ed25519 public key), QUIC transport with NAT hole-punching, and discovery via n0-operated free public relays. We don't operate any of this — n0 does, free, for the world. A future setting can point at self-hosted relays for the privacy-sensitive; out of scope for v1.

Direct P2P happens whenever NAT permits it. When it doesn't, the relay forwards encrypted bytes (it cannot read them — QUIC handshake is end-to-end). Transfers are content-addressed (BLAKE3) and resumable: a 4 GB upload that drops at 60% picks up where it left off.

### First pair: ticket or QR

The first time two krills meet, they exchange a `NodeTicket` — a base32 string (~70 chars) carrying the receiver's NodeID and current relay info. Two ways to share it:

- **QR code** for in-person pairing. The receiver's window shows a QR; the sender scans (webcam) or, if both are at the same machine, just clicks-and-pastes.
- **Copy & paste** for remote pairing. Receiver clicks "copy ticket," pastes it into Signal / iMessage / SMS to the sender, sender pastes into their app's input.

A ticket isn't an ephemeral one-time code — it's a serialization of your stable NodeID. Sharing it is equivalent to sharing your File Drop identity for this app forever (similar to handing out your phone number). To revoke, rotate the keypair from your own card; this orphans saved contacts on the other side, who'll see a "different NodeID" warning the next time you try to connect.

### Re-pair: just dial

For *saved* contacts there's no ticket exchange. The sender clicks the contact tile → Iroh's discovery layer locates the peer via the relay → if their app is running, the receiver's app prompts `Filip wants to send — accept?` (or auto-accepts, if they've enabled that for this contact).

If the receiver isn't running File Drop, the dial fails clean: `Tokyo is offline. Ask them to open File Drop, then try again.` The app never pretends to deliver to an offline peer.

### Identity & verification

Each install generates an ed25519 keypair on first launch via Iroh's `SecretKey`. The public key is the NodeID — Iroh's identity *is* our identity, no separate layer. Saved contacts are pinned by NodeID; if a saved peer's NodeID ever changes (key rotated, fresh install, impersonation attempt), the app shows a warning and refuses to auto-trust until the user explicitly re-pairs.

### Contacts (Option 2 — verification + direct dial)

After the first successful pair, both sides see "save as contact?" — stored as the peer's contact card plus `lastPaired` timestamp. From then on:

- **Contact tile is the drop target.** Click → dial → drag a file → done (with receiver's accept prompt, unless they've enabled auto-accept for you).
- **Live online / offline state.** Tile shows whether the peer's app is reachable via the relay right now. Greyed out when offline.
- **Verified-on-rematch.** Each connection re-confirms the saved NodeID. Mismatch raises a warning.

This is Option 2 with the bonus that Iroh's discovery makes the "click contact, it just works" path possible in v1 without us building presence ourselves.

## Contact cards

A contact card is a tiny self-authored JSON document representing *you*. It's exchanged automatically as the very first thing on every successful connection.

### Schema

```json
{
  "version": 1,
  "nodeId": "abcd1234...base32",
  "displayName": "Filip",
  "icon": "lucide:user",
  "avatar": "data:image/jpeg;base64,..."
}
```

- `nodeId` — derived from the keypair, not user-editable.
- `displayName` — short, freeform. Default on first launch: the user's login name (`$USER`), falling back to the device hostname. Editable any time.
- `icon` — Lucide icon name. Default: `user`. Shown as the card's visual identity when no `avatar` is set, and as a small badge alongside the avatar when one is set. Picked from a small curated palette at first launch.
- `avatar` *(optional)* — base64-encoded JPEG, 256×256, ≤ 256 KB. Center-cropped and resized at upload time; no manual cropper in v1. When present, it's the primary visual on the card and tile; when absent, the Lucide `icon` fills that slot.

Stored as `$XDG_STATE_HOME/krill-file-drop/identity.json` alongside the private key (the key file is `0600`).

### Card exchange during pairing

When two krills connect (first time or n-th time), the very first thing they do over the encrypted channel is exchange contact cards. So:

- **Receiver's experience.** They never type a nickname for the sender. The sender's card arrives pre-filled — name, icon, NodeID — and the receiver clicks "Save contact" if they want to keep it.
- **Sender's experience.** Symmetric. Receiver's card arrives pre-filled.
- **Both can override locally.** If filip's card says "Filip's laptop" but you want to save it as "filip-work," that's a local nickname stored on your side; it doesn't change his card. When his card updates (he renames himself), your nickname stays put.

### What if the user hasn't set up a card yet?

First launch flow: before the main window opens, File Drop walks the user through *one* short setup screen with three fields:

- **Name** — text input, pre-filled with `$USER` or hostname.
- **Icon** — visual picker, ~16 Lucide icons, default selection on `user`.
- **Photo** *(optional)* — drop or click to upload; auto-cropped to square, resized to 256×256 JPEG. "Skip" is fine — the icon fills the slot.

One screen, one click to "Done." This is the only time the app is modal.

After that the card lives in `identity.json` and can be edited from a small "your card" tile in the main window — click to swap the photo, change the name, or pick a different icon. No separate settings panel.

## The flow as users see it

**Setup (once per install).** Pick display name and icon. Done.

### First pair

**Tokyo (receiver):**
1. Opens File Drop.
2. Sees `Share to receive:` with a QR code and a `Copy ticket` button.
3. Either shows the QR (in person) or pastes the ticket into Signal to Stockholm.
4. When Stockholm's app dials, sees `Filip wants to send — accept?` → accepts → connection established.
5. Sees Stockholm's card appear; clicks "Save contact" to keep them.
6. Files arrive in `~/Downloads/krill-file-drop/`.

**Stockholm (sender):**
1. Opens File Drop.
2. Pastes Tokyo's ticket (or scans QR).
3. Connection establishes; Tokyo's card appears as a drop tile.
4. Drags `photo.jpg`. Progress overlay → done.
5. Clicks "Save contact" to keep Tokyo for next time.
6. Drags more files. Same connection.
7. Closes the window. Session ends.

### Subsequent transfer (Tokyo is now a saved contact)

**Stockholm:**
1. Opens File Drop. Tokyo's tile shows ● (online).
2. Drags `report.pdf` directly onto Tokyo's tile.
3. Tokyo's app shows the accept prompt; Tokyo accepts (or has auto-accept on).
4. Transfer happens. Done.

No ticket. No code. The whole interaction is one drag.

## What v1 is

- First pair via ticket or QR; saved contacts dial directly with no re-pairing.
- P2P transfer over QUIC with NAT hole-punching; n0 relay fallback when direct fails.
- End-to-end encrypted — relays cannot read content.
- Resumable large-file transfers (BLAKE3 content-addressed).
- Contact cards: self-authored on first launch, exchanged on every connect, saveable by either side.
- Saved contacts shown with live online/offline state, verified-on-rematch, warning on key mismatch.
- Per-contact auto-accept toggle on the receiver's side.
- Multiple files per session.
- Linux x86_64. Tauri 2 + TypeScript + Rust. Same stack as every other krill app.

## What v1 is *not*

- **No store-and-forward.** If the receiver isn't running File Drop right now, the sender waits. We never hold anyone's files.
- **No folder send.** Single files (or multiple single files in sequence). Folders → user zips first.
- **No file history / inbox / log.** Drops land in `~/Downloads/krill-file-drop/`; that directory *is* the inbox.
- **No chat, no message attachments.** Files only.
- **No krill-operated servers.** v1 uses n0's public relays. Self-hosting is a documented future option, not a v1 feature.
- **No iOS/Android client.** Receiving on a phone is a future question.
- **No macOS or Windows build.** Deferred, not rejected — see Future.
- **No accounts, no telemetry, no analytics.** Same as every krill app.

## Future (deferred decisions, not roadmap)

These are flagged here so future-us doesn't have to relitigate that they were considered.

- **macOS / Windows builds.** File Drop is the one krill app where being on more platforms compounds the value. If user feedback after v1 shows demand, the right path is likely a *separate project* sharing protocol code via a Rust crate, not retrofitting cross-platform conditionals into the krill tree.
- **Mobile client.** A receive-on-phone flow is the obvious natural extension. Out of scope for v1; Iroh has Rust libraries that could power one later.
- **Self-hosted relays.** A setting pointing the app at a user's own Iroh relay. Defers nicely on top of v1.
- **Identity rotation UX.** Right now rotating your keypair silently orphans saved contacts. A "broadcast new key to old contacts" flow would smooth this — out of scope for v1.

## Stack

- **Transport, identity, discovery:** [`iroh`](https://crates.io/crates/iroh) + [`iroh-blobs`](https://crates.io/crates/iroh-blobs) (Rust, MIT/Apache-2.0). Provides QUIC P2P, NAT hole-punching, NodeID identity, content-addressed resumable transfer.
- **State:** `$XDG_STATE_HOME/krill-file-drop/`
  - `identity.json` — your card metadata + private key (`0600`)
  - `contacts.json` — saved peers (one entry per peer; each includes the peer's last-known card and `lastPaired`)
  - `state.json` — window geometry
- **Files received:** `$XDG_DOWNLOAD_DIR/krill-file-drop/` (or `~/Downloads/krill-file-drop/` if XDG isn't set). Never overwrite — append `(2)` etc.
- **QR scanning:** webcam via `nokhwa` or similar Rust crate; only used when the user clicks "scan."
- **UI:** plain TypeScript + Vite, like every other krill app.

## Layout sketch

```
+--- File Drop --------------------------------------+
| [≡]                          [_] [□] [×]           |  <- titlebar
+----------------------------------------------------+
|                                                    |
|   YOUR CARD                                        |
|   ┌─────────────────────┐                          |
|   │ [icon] Filip's      │  click to edit           |
|   │        laptop       │                          |
|   └─────────────────────┘                          |
|                                                    |
|   ─────────────────────────────────────────────    |
|                                                    |
|   PAIR WITH SOMEONE NEW                            |
|                                                    |
|   I'm receiving →   [ QR code ]                    |
|                     [ copy ticket ]                |
|                                                    |
|   I'm sending  →   [ paste ticket: __________ ]    |
|                     [ scan QR ]                    |
|                                                    |
|   ─────────────────────────────────────────────    |
|                                                    |
|   CONTACTS                                         |
|   ┌────────────┐  ┌────────────┐  ┌────────────┐   |
|   │ ● [icon]   │  │ ○ [icon]   │  │ ○ [icon]   │   |
|   │   mom      │  │   sara     │  │   work     │   |
|   │   online   │  │   offline  │  │   offline  │   |
|   └────────────┘  └────────────┘  └────────────┘   |
|                                                    |
+----------------------------------------------------+
|  Click an online contact, or pair to send.         |  <- status line
+----------------------------------------------------+
```

When a contact is online and the user drags a file onto their tile, the tile expands into a progress overlay for the in-flight transfer. Multiple in-flight transfers stack vertically inside the tile.

## Resolved decisions

These were open questions during scoping; recording the calls so future-us doesn't re-relitigate them.

- **First-pair UI surface.** Show QR and "copy ticket" equally side-by-side. User picks the right channel for their context.
- **Auto-accept.** Off by default for every saved contact; user flips per-contact from the contact tile.
- **Transfer cancellation.** Hover the in-flight progress overlay → small × button.
- **Online indicator.** Strict — live state from Iroh's discovery. False negatives are fine; false positives (clicked an "online" contact that turns out to be offline) are not.
- **Card visual.** Optional uploaded avatar (JPEG, center-cropped, ≤ 256 KB) takes precedence over the Lucide icon. Default Lucide icon when no avatar is set: `user`.
- **First-run icon palette.** ~16 curated Lucide icons. Devices: `laptop`, `smartphone`, `tablet`, `monitor`. Places: `home`, `briefcase`, `cloud`, `mountain`. Atmospherics: `sun`, `moon`, `star`, `coffee`. Creatures: `cat`, `dog`, `heart`, `gamepad-2`. Default selection: `user` (always present alongside the 16 as the safe default).

## Open questions

(none currently — re-open as scoping reveals more)

## Milestones

- **M1 — Card + pair & echo.** First-run card setup (name, icon, optional avatar). Two File Drop instances pair via ticket, exchange contact cards, render each other's name + icon/avatar. No file transfer yet.
- **M2 — Single file transfer.** Drop one file onto a connected peer; arrives on the other side. P2P + relay fallback both work; transfer resumes after mid-flight network drop.
- **M3 — Contacts & dial.** Save-after-pair, online/offline state on contact tiles, click-to-dial saved contacts, accept prompt on receiver, per-contact auto-accept toggle, verified-on-rematch, key-mismatch warning.
- **M4 — Polish & packaging.** QR rendering and webcam scanning, multiple files per session, transfer cancellation (× on overlay), error states (peer offline, network drop, etc.), AppImage + .deb build, GitHub release workflow + docs landing page.
