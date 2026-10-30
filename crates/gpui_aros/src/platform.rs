//! The AROS platform: owns the run loop, dispatcher, display and text system.

use std::ffi::CString;
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::channel::oneshot;
use gpui::{
    Action, ActivityGuard, AnyWindowHandle, BackgroundExecutor, ClipboardItem, CursorStyle, DummyKeyboardMapper,
    ForegroundExecutor, Keymap, Menu, MenuItem, OperatingSystem, PathPromptOptions, Platform,
    PlatformDisplay, PlatformInfo, PlatformTarget,
    PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow, Task,
    ThermalState, WindowAppearance, WindowParams,
};

use crate::dispatcher::ArosDispatcher;
use crate::display::ArosDisplay;
use crate::menus::{MenuState, to_latin1};
use crate::text;
use crate::requesters::{RequestThreads, path_cstring, path_from_bytes};
use crate::window::{ArosWindow, ArosWindowInner};

// ~30fps: the run loop parks at most this long between frames.
const FRAME_INTERVAL_MS: i32 = 33;

#[derive(Default)]
struct PlatformCallbacks {
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    quit: Option<Box<dyn FnMut() -> bool>>,
    system_sleep: Option<Box<dyn FnMut()>>,
    system_wake: Option<Box<dyn FnMut()>>,
    reopen: Option<Box<dyn FnMut()>>,
    will_open_app_menu: Option<Box<dyn FnMut()>>,
    validate_app_menu_command: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
    keyboard_layout_change: Option<Box<dyn FnMut()>>,
    thermal_state_change: Option<Box<dyn FnMut()>>,
}

pub struct ArosPlatform {
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    dispatcher: Arc<ArosDispatcher>,
    text_system: Arc<dyn PlatformTextSystem>,
    display: Rc<dyn PlatformDisplay>,
    windows: RefCell<Vec<Weak<ArosWindowInner>>>,
    /// Shared with every window: Intuition activation events keep it true.
    active_window: Rc<Cell<Option<AnyWindowHandle>>>,
    callbacks: RefCell<PlatformCallbacks>,
    menu_state: Rc<RefCell<MenuState>>,
    quit_requested: Cell<bool>,
    cursor_visible: Cell<bool>,
    requesters: RefCell<RequestThreads>,
    #[allow(dead_code)]
    headless: bool,
}

impl ArosPlatform {
    pub fn new(headless: bool) -> Self {
        // Record the main task + allocate the run-loop wake signal before
        // any dispatcher thread exists that could try to Signal() it.
        // SAFETY: FFI; platform creation happens on the main thread.
        unsafe { crate::glue::gpa_init_main() };
        let dispatcher = Arc::new(ArosDispatcher::new());
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        let text_system = text::new_text_system();
        let display: Rc<dyn PlatformDisplay> = Rc::new(ArosDisplay::new());

        Self {
            background_executor,
            foreground_executor,
            dispatcher,
            text_system,
            display,
            windows: RefCell::new(Vec::new()),
            active_window: Rc::new(Cell::new(None)),
            callbacks: RefCell::new(PlatformCallbacks::default()),
            menu_state: Rc::new(RefCell::new(MenuState::default())),
            quit_requested: Cell::new(false),
            cursor_visible: Cell::new(true),
            requesters: RefCell::new(RequestThreads::default()),
            headless,
        }
    }

    /// Run every queued main-thread runnable. Runnables may open windows or
    /// request quit, so this holds no other borrow.
    fn drain_main_thread(&self) {
        let queue = self.dispatcher.main_queue();
        loop {
            let runnable = queue.lock().pop_front();
            match runnable {
                Some(runnable) => {
                    runnable.run();
                }
                None => break,
            }
        }
    }

    /// Live windows, with dead weaks pruned.
    fn live_windows(&self) -> Vec<Rc<ArosWindowInner>> {
        let mut windows = self.windows.borrow_mut();
        windows.retain(|w| w.strong_count() > 0);
        windows.iter().filter_map(Weak::upgrade).collect()
    }

    fn can_exit(&self) -> bool {
        let (idle, panics) = self.requesters.borrow_mut().reap_finished();
        if panics != 0 {
            log::error!("gpui_aros: {panics} native requester workers panicked");
        }
        self.quit_requested.get() && idle
    }

    fn request<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> oneshot::Receiver<Result<T>> {
        let (sender, receiver) = oneshot::channel();
        let sender = Arc::new(parking_lot::Mutex::new(Some(sender)));
        let worker_sender = sender.clone();
        let spawned = self.requesters.borrow_mut().spawn(move || {
            let result = work();
            let sender = worker_sender.lock().take();
            if let Some(sender) = sender {
                if sender.send(result).is_err() {
                    log::debug!("gpui_aros: requester caller closed before completion");
                }
            }
        });
        if let Err(error) = spawned {
            let sender = sender.lock().take();
            if let Some(sender) = sender {
                if sender.send(Err(error.into())).is_err() {
                    log::debug!("gpui_aros: requester caller closed before spawn failure");
                }
            }
        }
        receiver
    }
}

impl Platform for ArosPlatform {
    /// Native AROS identity. Keyboard binding families do not change the OS.
    fn platform_info(&self) -> PlatformInfo {
        PlatformInfo {
            os: OperatingSystem::Aros,
            target: PlatformTarget::Native,
        }
    }

    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        // GPUI_AROS_PROFILE=<file>: sample the main task's program counter
        // every millisecond until the run loop ends, then write the samples
        // to that file (symbolized on the host against the unstripped
        // binary; see the README).
        let profile = std::env::var_os("GPUI_AROS_PROFILE").filter(|v| !v.is_empty());
        let profile_path = profile
            .as_ref()
            .and_then(|p| CString::new(p.to_string_lossy().into_owned()).ok());
        if profile.is_some() {
            // SAFETY: FFI; the main task is recorded by gpa_init_main.
            let started = unsafe { crate::glue::gpa_profile_start(1000) };
            log::info!("gpui_aros profile: sampler started={started}");
        }
        let mut profile_flushed = Instant::now();
        on_finish_launching();

        while !self.can_exit() {
            let frame_start = Instant::now();
            if let Some(path) = &profile_path {
                // The samples so far every thirty seconds: a profile of a
                // session that never ends cleanly is still a profile.
                if profile_flushed.elapsed() >= Duration::from_secs(30) {
                    profile_flushed = Instant::now();
                    // SAFETY: FFI; the sampler is ours and the path is a C string.
                    unsafe { crate::glue::gpa_profile_flush(path.as_ptr()) };
                }
            }

            self.drain_main_thread();
            if self.can_exit() {
                break;
            }

            let windows = self.live_windows();
            for window in &windows {
                window.poll_events();
            }
            for window in &windows {
                window.request_frame();
            }
            if self.can_exit() {
                break;
            }

            // Park on the union of window signal masks — plus the dispatcher
            // wake signal, so dispatch_on_main_thread cuts the park short —
            // up to the frame budget.
            let mask = windows.iter().fold(0u32, |acc, w| acc | w.sigmask())
                // SAFETY: FFI; returns 0 before gpa_init_main.
                | unsafe { crate::glue::gpa_wake_sigmask() };
            let elapsed = frame_start.elapsed();
            let budget = Duration::from_millis(FRAME_INTERVAL_MS as u64);
            let remaining = budget.saturating_sub(elapsed).as_millis() as i32;
            if remaining > 0 {
                // SAFETY: FFI; mask==0 degrades to a plain sleep.
                unsafe { crate::glue::gpa_wait_timeout_ms(mask, remaining) };
            }
        }

        // Hand control back to GPUI so it can shut down: the callback
        // installed by `App` runs the `on_app_quit` observers, clears the
        // windows and flushes effects. Without this the whole shutdown phase
        // was skipped on AROS — quit observers never fired (so apps never got
        // their save-on-quit), windows were never torn down, and any entity
        // handle an observer would have released was still alive when `App`
        // dropped, which is what tripped GPUI's leak detector on exit.
        //
        // It runs *here*, at the end of the loop, rather than in `quit()`:
        // `App::quit` calls `Platform::quit` with the app state already
        // borrowed, and shutdown re-enters to run the observers, so calling
        // it there would double-borrow. macOS sidesteps the same trap by
        // deferring termination to a later run-loop turn; we defer to the end
        // of ours. Taking the callback out of the `RefCell` first keeps it
        // free to re-enter platform methods (closing windows does).
        let on_quit = self.callbacks.borrow_mut().quit.take();
        if let Some(mut on_quit) = on_quit {
            on_quit();
        }
        // Quit callbacks finish while dispatch is still available. Then stop
        // admission, service remaining foreground completions and join every
        // dispatcher-owned worker before this native command can return.
        self.dispatcher.shutdown();
        if let Some(path) = profile_path {
            // SAFETY: FFI; the sampler is ours and the path is a C string.
            let n = unsafe { crate::glue::gpa_profile_stop(path.as_ptr()) };
            log::info!("gpui_aros profile: {n} samples written");
        }
        // The run loop is over: its wake signal and timer go back to Exec.
        // SAFETY: FFI; main thread, after the last wait of the run loop.
        unsafe { crate::glue::gpa_shutdown_main() };
    }

    fn quit(&self) {
        // Quit observers can reenter Platform after the event loop exits.
        // Refuse new native work before waiting for the admitted workers.
        self.requesters.borrow_mut().close_admission();
        self.quit_requested.set(true);
    }

    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<OsString>) {}

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.active_window.get()
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        let window =
            ArosWindow::open(handle, options, self.display.clone(), self.menu_state.clone(), self.active_window.clone())?;
        self.windows.borrow_mut().push(Rc::downgrade(&window.inner()));
        self.active_window.set(Some(handle));
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        WindowAppearance::Light
    }

    fn open_url(&self, _url: &str) {}

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.callbacks.borrow_mut().open_urls = Some(callback);
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn prompt_for_paths(
        &self,
        options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let dirs_only = options.directories && !options.files;
        let title = options
            .prompt
            .as_ref()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "Select".into());
        let request = AslRequest {
            save_mode: false,
            drawers_only: dirs_only,
            multiselect: options.multiple,
            initial_drawer: None,
            initial_file: None,
            title,
        };
        self.request(move || asl_request(request))
    }

    fn prompt_for_new_path(
        &self,
        directory: &Path,
        suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let request = AslRequest {
            save_mode: true,
            drawers_only: false,
            multiselect: false,
            initial_drawer: Some(directory.to_path_buf()),
            initial_file: suggested_name.map(ToOwned::to_owned),
            title: "Save As".into(),
        };
        self.request(move || asl_request(request).map(|paths| paths.and_then(|mut paths| paths.pop())))
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {}

    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        self.callbacks.borrow_mut().quit = Some(callback);
    }

    /// AROS exposes no system sleep or wake notification to a program; the
    /// callbacks are kept and never fire.
    fn on_system_sleep(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().system_sleep = Some(callback);
    }

    fn on_system_wake(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().system_wake = Some(callback);
    }

    /// Idle-sleep inhibition is not available on AROS: the caller learns it
    /// from the error instead of holding a guard that does nothing.
    fn prevent_idle_sleep(&self, reason: &str) -> Task<Result<ActivityGuard>> {
        Task::ready(Err(anyhow::anyhow!(
            "idle sleep inhibition is not available on AROS (requested for: {reason})"
        )))
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().reopen = Some(callback);
    }

    fn set_menus(&self, menus: Vec<Menu>, keymap: &Keymap) {
        self.menu_state.borrow_mut().rebuild(menus, keymap);
        // The strip is per window on Amiga: apply to every live window;
        // windows opened later pick the spec up in ArosWindow::open.
        for window in self.live_windows() {
            window.apply_menus();
        }
    }

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.menu_state.borrow_mut().on_action = Some(callback);
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().will_open_app_menu = Some(callback);
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.callbacks.borrow_mut().validate_app_menu_command = Some(callback);
    }

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().thermal_state_change = Some(callback);
    }

    fn compositor_name(&self) -> &'static str {
        "AROS"
    }

    fn app_path(&self) -> Result<PathBuf> {
        Ok(std::env::current_exe()?)
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow::anyhow!(
            "path_for_auxiliary_executable is not available on AROS"
        ))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        // GPA_PTR_* codes in the C glue's pointerclass set.
        let code = match style {
            CursorStyle::Arrow => 0,
            CursorStyle::IBeam | CursorStyle::IBeamCursorForVerticalLayout => 1,
            CursorStyle::Crosshair => 2,
            CursorStyle::PointingHand
            | CursorStyle::OpenHand
            | CursorStyle::ClosedHand
            | CursorStyle::DragLink
            | CursorStyle::DragCopy
            | CursorStyle::ContextualMenu => 3,
            CursorStyle::ResizeLeft
            | CursorStyle::ResizeRight
            | CursorStyle::ResizeLeftRight
            | CursorStyle::ResizeColumn => 4,
            CursorStyle::ResizeUp
            | CursorStyle::ResizeDown
            | CursorStyle::ResizeUpDown
            | CursorStyle::ResizeRow => 5,
            CursorStyle::ResizeUpLeftDownRight => 6,
            CursorStyle::ResizeUpRightDownLeft => 7,
            CursorStyle::OperationNotAllowed => 8,
        };
        for window in self.live_windows() {
            window.set_pointer(code);
        }
    }

    fn hide_cursor_until_mouse_moves(&self) {
        self.cursor_visible.set(false);
    }

    fn is_cursor_visible(&self) -> bool {
        self.cursor_visible.get()
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        false
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        let mut buf: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len: std::os::raw::c_int = 0;
        // SAFETY: FFI; on success `buf` is an AllocVec'd buffer of `len`
        // bytes that we must hand back to gpa_free.
        unsafe {
            if crate::glue::gpa_clipboard_read_text(&mut buf, &mut len) != 0 || buf.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(buf as *const u8, len.max(0) as usize);
            // FTXT is system-charset text — ISO-8859-1 on stock AROS, which
            // maps 1:1 onto the first 256 Unicode scalars.
            let text: String = bytes.iter().map(|&b| b as char).collect();
            crate::glue::gpa_free(buf);
            Some(ClipboardItem::new_string(text))
        }
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        let Some(text) = item.text() else {
            return; // image-only clips: nothing to publish as FTXT
        };
        // UTF-8 -> ISO-8859-1, lossy: anything past U+00FF becomes '?'
        // (FTXT has no wider charset; a CSET/UTF8 chunk is a v2 nicety).
        let bytes: Vec<u8> = text
            .chars()
            .map(|c| if (c as u32) <= 0xFF { c as u32 as u8 } else { b'?' })
            .collect();
        // SAFETY: FFI; the glue copies the bytes before returning.
        unsafe {
            if crate::glue::gpa_clipboard_write_text(
                bytes.as_ptr() as *const std::ffi::c_void,
                bytes.len() as std::os::raw::c_int,
            ) != 0
            {
                log::warn!("gpui_aros: clipboard write failed");
            }
        }
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(ArosKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().keyboard_layout_change = Some(callback);
    }

    /// The backend shows its windows on one screen chosen at start-up, so
    /// the set of displays it reports does not change.
    fn on_displays_changed(&self, _callback: Box<dyn FnMut()>) {}
}

struct ArosKeyboardLayout;

impl PlatformKeyboardLayout for ArosKeyboardLayout {
    fn id(&self) -> &str {
        "us"
    }

    fn name(&self) -> &str {
        "US"
    }
}

struct AslRequest {
    save_mode: bool,
    drawers_only: bool,
    multiselect: bool,
    initial_drawer: Option<PathBuf>,
    initial_file: Option<String>,
    title: String,
}

/// Run the blocking asl.library file requester and decode the glue's
/// NUL-separated path list. DOS filenames are bytes, including UTF-8 or
/// legacy names; only the requester's display title uses the UI charset.
fn asl_request(req: AslRequest) -> Result<Option<Vec<PathBuf>>> {
    let drawer = path_cstring(req.initial_drawer.as_deref().unwrap_or(Path::new("")))?;
    let file = CString::new(req.initial_file.as_deref().unwrap_or("").as_bytes())?;
    let title = CString::new(to_latin1(&req.title))?;

    let mut buf: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut len: std::os::raw::c_int = 0;
    // SAFETY: FFI; on success `buf` is an AllocVec'd NUL-separated,
    // double-NUL-terminated list we must hand back to gpa_free.
    unsafe {
        let result = crate::glue::gpa_asl_request_paths(
            req.save_mode as _,
            req.drawers_only as _,
            req.multiselect as _,
            drawer.as_ptr(),
            file.as_ptr(),
            title.as_ptr(),
            &mut buf,
            &mut len,
        );
        if result == 1 {
            return Ok(None);
        }
        if result != 0 || buf.is_null() || len < 2 {
            if !buf.is_null() {
                crate::glue::gpa_free(buf);
            }
            anyhow::bail!("AROS file requester failed");
        }
        let bytes = std::slice::from_raw_parts(buf as *const u8, len.max(0) as usize);
        let paths: Vec<PathBuf> = bytes
            .split(|&b| b == 0)
            .filter(|part| !part.is_empty())
            .map(path_from_bytes)
            .collect();
        crate::glue::gpa_free(buf);
        Ok((!paths.is_empty()).then_some(paths))
    }
}
