# Quick Ask

A small native macOS chat panel for quick questions. Written in Rust with GPUI,
rendered with Metal, and kept in the menu bar. Press **Option+Space**, type a
question, and press **Enter**. The panel expands as the answer streams in.

On macOS 26 and newer, the panel uses AppKit's native `NSGlassEffectView` with
system appearance, a compact icon toolbar, and content-sized answers. Older
systems fall back to macOS vibrancy.

## Use

1. Open `Quick Ask.app`, or run the development command below.
2. Select **Continue with ChatGPT** and finish signing in in your browser.
3. Allow the app to use your ChatGPT plan if that permission is offered.
4. Choose an available model in the footer and ask a question.

The app has been tested with browser sign-in, Keychain credential persistence,
the account's live model catalog, and a real subscription-backed reply.
This first version keeps one saved account registration; an account picker is
not implemented. Sign out disconnects that registration's local session.

Use your personal browser profile for a personal Plus/Pro account. A work
browser session may select your organization's account instead; its administrator
can restrict third-party ChatGPT sign-in. **Cancel** in Quick Ask stops a pending
sign-in so you can switch browser profiles and try again.

| Shortcut | Action |
| --- | --- |
| Option+Space | Show or hide the panel from any app |
| Control+Option+Space | Automatic fallback if Option+Space is unavailable |
| Enter | Send the question |
| Shift+Enter | Add a new line |
| Escape | Dismiss the panel |
| Command+N | Start a new chat |
| Command+H | Open local history |
| Command+Shift+C | Copy the current or latest answer |
| Command+Q | Quit Quick Ask |

The keyboard shortcuts other than the global show/hide shortcut apply while the
panel has focus. Click **Stop** to cancel a response and close its connection.
Partial responses are shown until you start another turn, but are not saved or
included in later model context. Clicking another app or
pressing Escape dismisses the panel; Quick Ask stays in the menu bar.

The menu bar menu also provides **Show / Hide**, **New Chat**, **Manage ChatGPT
Usage**, and **Quit**. If another app owns Option+Space, Quick Ask shows a notice
and uses Control+Option+Space. Disable the conflicting shortcut in ChatGPT
Classic, Raycast, or the app using it, then reopen Quick Ask to reclaim
Option+Space. A running Quick Ask instance must quit before another starts.

## Subscription and local data

Quick Ask uses **Sign in with ChatGPT** and requests permission to use an eligible
ChatGPT plan; it does not ask for an API key. These requests consume the same
**Codex / ChatGPT Work allowance** included in your plan. Connecting an app adds
no separate allowance. Access and models depend on the account and its policies.
Review app limits and permissions in [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage).
See the [official OpenAI documentation](https://learn.chatgpt.com/docs/sign-in-with-chatgpt)
for current eligibility and plan-sharing behavior.

Plan access does not include your ChatGPT conversations or memories. Quick Ask
keeps its own chat history on this Mac and sends the selected conversation as
context for each reply. Its chats do not appear in your ChatGPT history, and it
does not load ChatGPT memory.

Data is stored in `~/Library/Application Support/Quick Ask/`:

- `history.json`: up to 100 recent local conversations, saved with owner-only
  permissions. This is readable local JSON, not encrypted storage.
- `chatgpt-account.json`: local app registration and account metadata.
- `app.lock`: prevents competing instances from refreshing the same session.

Access and refresh credentials are stored in **macOS Keychain**, not in the chat
history file. Signing out disconnects the saved session in this app and keeps
local history. To remove that history, quit Quick Ask and delete `history.json`.
To revoke the app's permission, use ChatGPT Settings → Security and login →
Sign in with ChatGPT.

## Build and run

Use macOS with a Rust toolchain supporting edition 2024 and `std::fs::File`
locking, plus Xcode command line tools and the Metal compiler. The project was
built with Rust 1.99.1. The first build downloads and compiles GPUI dependencies.

```sh
xcode-select --install
rustup update stable
cargo build --package quick-ask
cargo run --package quick-ask
```

If Xcode reports that the Metal toolchain is missing, install it and retry:

```sh
xcodebuild -downloadComponent MetalToolchain
```

The app runs from the menu bar without a Dock icon. Quit the existing instance
before running a newly built version. An internet connection is needed to sign
in, fetch available models, and send messages.

## Test and package

```sh
cargo test --workspace
cargo check --workspace
cargo fmt --all -- --check
./scripts/package.sh
open "dist/Quick Ask.app"
```

The package script builds the release binary, draws the app icon with AppKit,
creates `dist/Quick Ask.app`, and signs it ad hoc for local use. It does not
install the app or create a notarized distribution. The icon source is
`scripts/generate-icon.swift`; regenerate the checked-in preview and icon with:

```sh
xcrun swift scripts/generate-icon.swift scripts/icons
```

For a faster bundle during development, run `./scripts/package.sh --debug`.

Protocol and storage tests use local fixtures. They do not substitute for a real
ChatGPT sign-in and live request. Text chat and local history are the scope of
this version; uploads, voice, web tools, and ChatGPT memory are not implemented.
