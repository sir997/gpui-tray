# gpui-tray

Cross-platform system tray support for apps using `gpui-kit`. No Zed Git checkout is required.

## Use

Add the dependency:

```toml
[dependencies]
gpui-kit = { version = "0.6.1", default-features = false }
gpui-tray = { git = "https://github.com/sir997/gpui-tray" }
```

Create and install a tray item:

```rust
use gpui_kit as gpui;
use gpui::App;
use gpui_tray::{
    TrayClickAction, TrayClickPolicy, TrayEvent, TrayMenuItem, TrayState,
};

fn main() -> anyhow::Result<()> {
    gpui::application().run(|cx: &mut App| {
        let async_app = cx.to_async();

        let state = TrayState::new()
            .visible(true)
            .icon(gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                include_bytes!("app-icon.png").to_vec(),
            ))
            .title("My App")
            .tooltip("Hello from tray")
            .click_policy(
                TrayClickPolicy::platform_default()
                    .left(TrayClickAction::EmitEvent)
                    .right(TrayClickAction::OpenMenu),
            )
            .submenu(TrayMenuItem::info("Connected to node-a"))
            .submenu(TrayMenuItem::menu("quit", "Quit", Vec::new()))
            .submenu(
                TrayMenuItem::menu("syncing", "Applying settings...", Vec::new()).enabled(false),
            );

        let tray = gpui_tray::tray::set_up_tray(cx, async_app, state, |event, cx| match event {
                TrayEvent::MenuClick { id } if id == "quit" => cx.quit(),
                _ => {}
            })
            .ok();

        if let Some(tray) = tray {
            let updated = TrayState::new()
                .visible(true)
                .title("My App (syncing)")
                .tooltip("Refreshing tray state");
            let _ = tray.set_state(updated);
            let _ = tray.flush_now(cx);
        }
    });
    Ok(())
}
```

Update the tray later by calling `tray.set_state(new_state)`, and call `tray.flush_now(cx)` when you want to eagerly push the latest desired state to the native tray.

Handles are main-thread-only. Call `tray.close(cx)` before discarding a tray;
dropping a handle alone does not close it. Closing invalidates all clones and
allows another tray to be created. On Linux, initialization, flushing and closing
are asynchronous D-Bus operations; errors are reported through the `log` facade.
Only one tray may be active at a time.

### Menu Item Capabilities

- `TrayMenuItem::menu(...).enabled(false)` renders a disabled native menu item.
- `TrayMenuItem::info(...)` and `TrayMenuItem::label(...)` create non-interactive text rows.
- `TrayMenuItem::menu(...).visible(false)` hides an item without removing it from your builder code.
- `TrayEvent::TrayClick` now includes a `kind` field so double-click policies can emit distinct events.

### Icon Notes

- `.icon(...)` takes anything convertible into `gpui::Image` (e.g. `gpui::Image::from_bytes(...)`).
- On macOS icons fit inside **18 × 18 logical points**, preserving aspect ratio. Pixel dimensions do not control the menu-bar size.
- Use a **36 × 36 pixel PNG** for a square Retina icon, with consistent transparent padding across states. Source pixels are not downsampled by this crate.
- `.icon_size(18.0)` changes the macOS logical bounding box; `.icon_template(false)` preserves colors instead of system tinting. Template mode defaults to true and adapts to light/dark appearance.
- Sizing and template flags are applied before assigning the image to the native button, on both initial creation and state updates. These flags have no effect on Windows/Linux.

## Run Demo

```bash
cargo run --example tray_demo
```
