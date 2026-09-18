#![allow(unsafe_op_in_unsafe_fn)]

use crate::tray::{
    TrayClickAction, TrayClickKind, TrayClickPolicy, TrayEvent, TrayEventCallback,
    TrayEventCallbackSlot, TrayMenuItem, TrayRuntimeState, TrayState, TrayToggleType,
    VersionedTrayState,
};
use anyhow::{Context as _, Result};
use gpui::{AsyncApp, MouseButton, Point};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, NSObject, Sel};
use objc2::{AnyThread, ClassType, MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSCellImagePosition, NSControlStateValueOff, NSControlStateValueOn, NSEvent,
    NSEventMask, NSEventType, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSSize, NSString};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    sync::{Arc, Mutex, OnceLock},
};

#[derive(Clone)]
pub struct TrayHandle {
    closed: std::rc::Rc<std::cell::Cell<bool>>,
}

impl TrayHandle {
    pub fn set_state(&self, state: TrayState) -> Result<()> {
        anyhow::ensure!(!self.closed.get(), "tray is closed");
        let async_app = TRAY_RUNTIME.with(|runtime_cell| -> Result<Option<AsyncApp>> {
            let mut runtime_slot = runtime_cell
                .try_borrow_mut()
                .map_err(|_| anyhow::anyhow!("tray runtime already borrowed"))?;
            let runtime = runtime_slot
                .as_mut()
                .context("tray has not been initialized")?;
            let should_schedule = runtime.state.set_desired_state(state);
            Ok(should_schedule.then(|| runtime.async_app.clone()))
        })?;

        if let Some(async_app) = async_app {
            schedule_flush(async_app, self.closed.clone());
        }

        Ok(())
    }

    pub fn flush_now(&self, _cx: &mut gpui::App) -> Result<()> {
        anyhow::ensure!(!self.closed.get(), "tray is closed");
        flush_runtime()
    }

    /// Removes the native tray and permits a new tray to be created.
    /// All clones of this handle become closed. Call on the GPUI thread.
    pub fn close(&self, _cx: &mut gpui::App) -> Result<()> {
        if self.closed.get() {
            return Ok(());
        }
        let runtime = TRAY_RUNTIME.with(|slot| -> Result<_> {
            let mut slot = slot
                .try_borrow_mut()
                .context("tray runtime already borrowed")?;
            anyhow::ensure!(
                !slot
                    .as_ref()
                    .is_some_and(|runtime| runtime.interaction_active),
                "tray menu is active"
            );
            Ok(slot.take())
        })?;
        self.closed.set(true);
        if let Some(runtime) = &runtime
            && let Some(platform) = &runtime.platform
            && let Ok(mut callback) = platform.handler.callback.try_lock()
        {
            *callback = None;
        }
        drop(runtime);
        Ok(())
    }
}

fn with_pool<T>(f: impl FnOnce() -> T) -> T {
    autoreleasepool(|_| f())
}

fn mtm() -> Result<MainThreadMarker> {
    MainThreadMarker::new().context("AppKit usage requires running on the main thread")
}

#[derive(Clone)]
struct Handler {
    async_app: AsyncApp,
    callback: TrayEventCallbackSlot,
    tag_to_id: Arc<Mutex<HashMap<i64, String>>>,
}

impl Handler {
    fn dispatch(&self, event: TrayEvent) {
        let async_app = self.async_app.clone();
        let executor = async_app.foreground_executor().clone();
        let callback = self.callback.clone();
        executor
            .spawn(async move {
                async_app.update(|cx| {
                    if let Ok(mut slot) = callback.lock()
                        && let Some(cb) = slot.as_mut()
                    {
                        cb(event, cx);
                    }
                });
            })
            .detach();
    }

    fn dispatch_tag(&self, tag: i64) {
        let id = self
            .tag_to_id
            .lock()
            .ok()
            .and_then(|map| map.get(&tag).cloned());
        if let Some(id) = id {
            self.dispatch(TrayEvent::MenuClick { id });
        }
    }
}

struct TargetState {
    handler: Handler,
}

struct StatusItemClickContext {
    handler: Handler,
    click_policy: TrayClickPolicy,
    status_item: Option<Retained<NSStatusItem>>,
    menu: Retained<NSMenu>,
}

struct TrayPlatform {
    mtm: MainThreadMarker,
    status_item: Option<Retained<NSStatusItem>>,
    menu: Retained<NSMenu>,
    target: Retained<AnyObject>,
    handler: Handler,
    click_policy: TrayClickPolicy,
}

struct TrayRuntime {
    closed: std::rc::Rc<std::cell::Cell<bool>>,
    async_app: AsyncApp,
    state: TrayRuntimeState,
    platform: Option<Box<TrayPlatform>>,
    interaction_active: bool,
}

thread_local! {
    static TRAY_RUNTIME: RefCell<Option<TrayRuntime>> = const { RefCell::new(None) };
}

impl Drop for TrayPlatform {
    fn drop(&mut self) {
        if let Some(item) = self.status_item.take() {
            NSStatusBar::systemStatusBar().removeStatusItem(&item);
        }
    }
}

fn target_class() -> Result<&'static AnyClass> {
    static CLASS: OnceLock<&'static AnyClass> = OnceLock::new();

    if let Some(class) = CLASS.get() {
        return Ok(class);
    }

    if let Some(existing) = AnyClass::get(c"GpuiTrayTarget") {
        let _ = CLASS.set(existing);
        return Ok(existing);
    }

    let class = {
        let mut builder = ClassBuilder::new(c"GpuiTrayTarget", NSObject::class())
            .context("failed to create Objective-C class declaration")?;

        builder.add_ivar::<*mut c_void>(c"rust_state");

        extern "C" fn on_menu_item(this: *mut NSObject, _cmd: Sel, sender: *mut AnyObject) {
            unsafe {
                let Some(this) = this.as_ref() else {
                    return;
                };
                if sender.is_null() {
                    return;
                }
                let cls = AnyClass::get(c"GpuiTrayTarget").unwrap();
                let ivar = cls.instance_variable(c"rust_state").unwrap();
                let state_ptr = *ivar.load::<*mut c_void>(this);
                if state_ptr.is_null() {
                    return;
                }

                let tag: i64 = msg_send![sender, tag];
                let state = &*(state_ptr as *const TargetState);
                state.handler.dispatch_tag(tag);
            }
        }

        extern "C" fn on_status_item_click(
            _this: *mut NSObject,
            _cmd: Sel,
            sender: *mut AnyObject,
        ) {
            if sender.is_null() {
                return;
            }

            let _ = handle_status_item_click();
        }

        extern "C" fn dealloc(this: *mut NSObject, _cmd: Sel) {
            unsafe {
                let Some(this_ref) = this.as_ref() else {
                    return;
                };
                let cls = AnyClass::get(c"GpuiTrayTarget").unwrap();
                let ivar = cls.instance_variable(c"rust_state").unwrap();
                let state_ptr = *ivar.load::<*mut c_void>(this_ref);
                if !state_ptr.is_null() {
                    drop(Box::from_raw(state_ptr as *mut TargetState));
                    *ivar.load_ptr::<*mut c_void>(this_ref) = std::ptr::null_mut::<c_void>();
                }
                let this_any: &mut AnyObject = &mut *this.cast::<AnyObject>();
                let _: () = msg_send![super(this_any, NSObject::class()), dealloc];
            }
        }

        unsafe {
            let on_menu_item_fn: extern "C" fn(*mut NSObject, Sel, *mut AnyObject) = on_menu_item;
            let on_status_item_click_fn: extern "C" fn(*mut NSObject, Sel, *mut AnyObject) =
                on_status_item_click;
            let dealloc_fn: extern "C" fn(*mut NSObject, Sel) = dealloc;
            builder.add_method::<NSObject, _>(sel!(onMenuItem:), on_menu_item_fn);
            builder.add_method::<NSObject, _>(sel!(onStatusItemClick:), on_status_item_click_fn);
            builder.add_method::<NSObject, _>(sel!(dealloc), dealloc_fn);
        }

        builder.register()
    };

    let _ = CLASS.set(class);
    Ok(class)
}

pub fn set_up_tray(
    cx: &mut gpui::App,
    async_app: AsyncApp,
    initial: TrayState,
    on_event: TrayEventCallback,
) -> Result<TrayHandle> {
    with_pool(|| unsafe {
        let mtm = mtm()?;
        let menu = NSMenu::new(mtm);

        let closed = std::rc::Rc::new(std::cell::Cell::new(false));
        let callback = Arc::new(Mutex::new(Some(on_event)));
        let tag_to_id = Arc::new(Mutex::new(HashMap::new()));
        let handler = Handler {
            async_app: async_app.clone(),
            callback,
            tag_to_id,
        };

        let state = Box::new(TargetState {
            handler: handler.clone(),
        });

        let target_class = target_class()?;
        let state_ptr = Box::into_raw(state) as *mut c_void;
        let target: Retained<AnyObject> = msg_send![target_class, new];
        let ivar = target_class.instance_variable(c"rust_state").unwrap();
        *ivar.load_ptr::<*mut c_void>(&target) = state_ptr;

        TRAY_RUNTIME.with(|runtime_cell| {
            let mut runtime_slot = runtime_cell
                .try_borrow_mut()
                .map_err(|_| anyhow::anyhow!("tray runtime already borrowed"))?;
            if runtime_slot.is_some() {
                anyhow::bail!("tray already initialized");
            }

            *runtime_slot = Some(TrayRuntime {
                closed: closed.clone(),
                async_app: async_app.clone(),
                state: TrayRuntimeState::new(initial),
                platform: Some(Box::new(TrayPlatform {
                    mtm,
                    status_item: None,
                    menu,
                    target,
                    handler,
                    click_policy: TrayClickPolicy::default(),
                })),
                interaction_active: false,
            });

            Ok(())
        })?;

        let handle = TrayHandle { closed };
        if let Err(error) = handle.flush_now(cx) {
            handle.close(cx)?;
            return Err(error);
        }
        Ok(handle)
    })
}

fn schedule_flush(async_app: AsyncApp, closed: std::rc::Rc<std::cell::Cell<bool>>) {
    let executor = async_app.foreground_executor().clone();
    executor
        .spawn(async move {
            if closed.get() {
                return;
            }
            if let Err(error) = flush_runtime() {
                log::error!("tray update failed: {error:#}");
            }
        })
        .detach();
}

fn handle_status_item_click() -> Result<()> {
    let mut platform = TRAY_RUNTIME.with(|runtime_cell| {
        let mut runtime_slot = runtime_cell
            .try_borrow_mut()
            .map_err(|_| anyhow::anyhow!("tray runtime already borrowed"))?;
        let runtime = runtime_slot
            .as_mut()
            .context("tray has not been initialized")?;
        runtime.interaction_active = true;
        runtime
            .platform
            .take()
            .context("tray platform missing during click")
    })?;

    let click_result = platform.handle_status_item_click();

    let async_app = TRAY_RUNTIME.with(
        |runtime_cell| -> Result<Option<(AsyncApp, std::rc::Rc<std::cell::Cell<bool>>)>> {
            let mut runtime_slot = runtime_cell
                .try_borrow_mut()
                .map_err(|_| anyhow::anyhow!("tray runtime already borrowed"))?;
            let runtime = runtime_slot
                .as_mut()
                .context("tray has not been initialized")?;
            runtime.platform = Some(platform);
            runtime.interaction_active = false;
            Ok(runtime
                .state
                .has_pending_flush()
                .then(|| (runtime.async_app.clone(), runtime.closed.clone())))
        },
    )?;

    if let Some((async_app, closed)) = async_app {
        schedule_flush(async_app, closed);
    }

    click_result
}

fn flush_runtime() -> Result<()> {
    with_pool(|| {
        loop {
            let step = TRAY_RUNTIME.with(
                |runtime_cell| -> Result<Option<(Box<TrayPlatform>, VersionedTrayState)>> {
                    let mut runtime_slot = runtime_cell
                        .try_borrow_mut()
                        .map_err(|_| anyhow::anyhow!("tray runtime already borrowed"))?;
                    let runtime = runtime_slot
                        .as_mut()
                        .context("tray has not been initialized")?;

                    if runtime.interaction_active {
                        return Ok(None);
                    }

                    let Some(versioned_state) = runtime.state.try_begin_flush() else {
                        return Ok(None);
                    };

                    let platform = runtime
                        .platform
                        .take()
                        .context("tray platform missing during flush")?;

                    Ok(Some((platform, versioned_state)))
                },
            )?;

            let Some((mut platform, versioned_state)) = step else {
                return Ok(());
            };

            let apply_result = platform.apply(&versioned_state.state);

            let should_continue = TRAY_RUNTIME.with(|runtime_cell| -> Result<bool> {
                let mut runtime_slot = runtime_cell
                    .try_borrow_mut()
                    .map_err(|_| anyhow::anyhow!("tray runtime already borrowed"))?;
                let runtime = runtime_slot
                    .as_mut()
                    .context("tray has not been initialized")?;
                runtime.platform = Some(platform);

                if apply_result.is_ok() {
                    Ok(runtime.state.finish_flush(versioned_state))
                } else {
                    runtime.state.abort_flush();
                    Ok(false)
                }
            })?;

            apply_result?;

            if !should_continue {
                return Ok(());
            }
        }
    })
}

impl TrayPlatform {
    fn status_item_click_context(&self) -> StatusItemClickContext {
        StatusItemClickContext {
            handler: self.handler.clone(),
            click_policy: self.click_policy,
            status_item: self.status_item.clone(),
            menu: self.menu.clone(),
        }
    }

    fn apply(&mut self, state: &TrayState) -> Result<()> {
        self.set_visible(state.visible)?;
        if !state.visible {
            return Ok(());
        }

        self.rebuild_menu(&state.submenus)?;

        let status_item = self.status_item.as_ref().context("status item is nil")?;
        let button = status_item
            .button(self.mtm)
            .context("status item button is nil")?;

        unsafe { button.setTarget(Some(&self.target)) };
        unsafe { button.setAction(Some(sel!(onStatusItemClick:))) };
        button.sendActionOn(
            NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp | NSEventMask::OtherMouseUp,
        );

        let tooltip = NSString::from_str(state.tooltip.as_str());
        button.setToolTip(Some(&tooltip));

        let title = NSString::from_str(state.title.as_str());
        button.setTitle(&title);

        let nsimage = state
            .icon
            .as_deref()
            .map(|image| nsimage_from_image(image, state.icon_size, state.icon_template))
            .transpose()?;
        if let Some(nsimage) = nsimage {
            button.setImage(Some(&nsimage));
            button.setImagePosition(NSCellImagePosition::ImageLeft);
        } else {
            button.setImage(None);
        }

        self.click_policy = state.click_policy;
        Ok(())
    }

    fn handle_status_item_click(&mut self) -> Result<()> {
        self.status_item_click_context().handle()
    }

    fn set_visible(&mut self, visible: bool) -> Result<()> {
        if visible {
            if self.status_item.is_some() {
                return Ok(());
            }

            let status_bar = NSStatusBar::systemStatusBar();
            let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
            self.status_item = Some(status_item);
        } else {
            if self.status_item.is_none() {
                return Ok(());
            }
            if let Some(item) = self.status_item.take() {
                NSStatusBar::systemStatusBar().removeStatusItem(&item);
            }
        }

        Ok(())
    }

    fn rebuild_menu(&mut self, items: &[TrayMenuItem]) -> Result<()> {
        with_pool(|| unsafe {
            self.menu.removeAllItems();

            if let Ok(mut map) = self.handler.tag_to_id.lock() {
                map.clear();
            }

            let mut next_tag: i64 = 1;
            for item in items {
                add_tray_menu_item(
                    &self.menu,
                    item,
                    &self.handler,
                    &self.target,
                    self.mtm,
                    &mut next_tag,
                )?;
            }

            Ok(())
        })
    }
}

impl StatusItemClickContext {
    fn handle(&self) -> Result<()> {
        let app = NSApplication::sharedApplication(mtm()?);
        let event = app
            .currentEvent()
            .context("status item click missing event")?;
        let mouse_location = NSEvent::mouseLocation();
        let position = Point {
            x: mouse_location.x as i32,
            y: mouse_location.y as i32,
        };

        let (action, button, kind) = match event.r#type() {
            NSEventType::RightMouseUp => (
                self.click_policy.right,
                MouseButton::Right,
                TrayClickKind::Single,
            ),
            NSEventType::LeftMouseUp if event.clickCount() >= 2 => (
                self.click_policy.double_click,
                MouseButton::Left,
                TrayClickKind::Double,
            ),
            NSEventType::LeftMouseUp => (
                self.click_policy.left,
                MouseButton::Left,
                TrayClickKind::Single,
            ),
            _ => return Ok(()),
        };

        match action {
            TrayClickAction::EmitEvent => {
                self.handler.dispatch(TrayEvent::TrayClick {
                    button,
                    kind,
                    position,
                });
            }
            TrayClickAction::OpenMenu => {
                if let Some(status_item) = self.status_item.as_ref() {
                    #[allow(deprecated)]
                    status_item.popUpStatusItemMenu(&self.menu);
                }
            }
            TrayClickAction::Ignore => {}
        }

        Ok(())
    }
}

fn nsimage_from_image(
    image: &gpui::Image,
    points: f64,
    template: bool,
) -> Result<Retained<NSImage>> {
    let nsdata = unsafe {
        NSData::dataWithBytes_length(image.bytes.as_ptr().cast(), image.bytes.len() as _)
    };
    let image = NSImage::initWithData(NSImage::alloc(), &nsdata)
        .context("failed to create NSImage from gpui::Image bytes")?;
    image.setSize(fit_icon(image.size(), points)?);
    image.setTemplate(template);
    Ok(image)
}

fn fit_icon(size: NSSize, points: f64) -> Result<NSSize> {
    anyhow::ensure!(
        size.width.is_finite() && size.height.is_finite() && size.width > 0.0 && size.height > 0.0,
        "invalid icon dimensions"
    );
    let scale = points / size.width.max(size.height);
    Ok(NSSize::new(size.width * scale, size.height * scale))
}

unsafe fn add_tray_menu_item(
    menu: &NSMenu,
    item: &TrayMenuItem,
    handler: &Handler,
    target: &AnyObject,
    mtm: MainThreadMarker,
    next_tag: &mut i64,
) -> Result<()> {
    match item {
        TrayMenuItem::Separator { visible, .. } => {
            if !*visible {
                return Ok(());
            }
            let separator = NSMenuItem::separatorItem(mtm);
            menu.addItem(&separator);
        }
        TrayMenuItem::Submenu {
            id: _,
            label,
            enabled,
            visible,
            role: _,
            toggle_type,
            children,
        } => {
            if !*visible {
                return Ok(());
            }

            if children.is_empty() {
                let title = NSString::from_str(label.as_str());
                let key_equiv = NSString::from_str("");
                let menu_item = NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &title,
                    Some(sel!(onMenuItem:)),
                    &key_equiv,
                );
                if let Some(user_id) = item.menu_event_id() {
                    let tag = *next_tag;
                    *next_tag += 1;

                    if let Ok(mut map) = handler.tag_to_id.lock() {
                        map.insert(tag, user_id.to_string());
                    }

                    unsafe { menu_item.setTarget(Some(target)) };
                    menu_item.setTag(tag as _);
                }

                let checked = match toggle_type {
                    Some(TrayToggleType::Checkbox(checked)) => *checked,
                    Some(TrayToggleType::Radio(checked)) => *checked,
                    None => false,
                };
                menu_item.setState(if checked {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                });
                menu_item.setEnabled(*enabled);
                menu.addItem(&menu_item);
            } else {
                let submenu = NSMenu::new(mtm);
                for child in children {
                    add_tray_menu_item(&submenu, child, handler, target, mtm, next_tag)?;
                }

                let title = NSString::from_str(label.as_str());
                let menu_item = NSMenuItem::new(mtm);
                menu_item.setTitle(&title);
                menu_item.setEnabled(*enabled);
                menu_item.setSubmenu(Some(&submenu));
                menu.addItem(&menu_item);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod icon_tests {
    use super::*;

    #[test]
    fn native_image_has_size_and_template_before_assignment() {
        autoreleasepool(|_| {
            let source = gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                include_bytes!("../../examples/app-icon.png").to_vec(),
            );
            let image = nsimage_from_image(&source, 18.0, true).unwrap();
            assert_eq!(image.size(), NSSize::new(18.0, 18.0));
            assert!(image.isTemplate());
            let image = nsimage_from_image(&source, 20.0, false).unwrap();
            assert_eq!(image.size(), NSSize::new(20.0, 20.0));
            assert!(!image.isTemplate());
        });
    }

    #[test]
    fn closed_handle_rejects_updates_and_invalidates_clones() {
        let handle = TrayHandle {
            closed: Default::default(),
        };
        let other = handle.clone();
        handle.closed.set(true);
        assert!(other.set_state(TrayState::new()).is_err());
    }

    #[test]
    fn pixel_density_does_not_change_logical_size() {
        for pixels in [18.0, 36.0, 72.0, 256.0] {
            assert_eq!(
                fit_icon(NSSize::new(pixels, pixels), 18.0).unwrap(),
                NSSize::new(18.0, 18.0)
            );
        }
    }

    #[test]
    fn keeps_aspect_ratio() {
        assert_eq!(
            fit_icon(NSSize::new(72.0, 36.0), 18.0).unwrap(),
            NSSize::new(18.0, 9.0)
        );
        assert_eq!(
            fit_icon(NSSize::new(36.0, 72.0), 18.0).unwrap(),
            NSSize::new(9.0, 18.0)
        );
    }

    #[test]
    fn rejects_empty_images() {
        assert!(fit_icon(NSSize::new(0.0, 36.0), 18.0).is_err());
    }
}
