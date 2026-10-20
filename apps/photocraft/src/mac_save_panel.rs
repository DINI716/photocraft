//! File › Save As on macOS with a Format pop-up under the name field, as Photoshop has (#6490).
//!
//! rfd's macOS save panel only restricts the allowed extensions; it shows no file type control,
//! so PDF, PNG and the rest could be reached only by typing the extension. This panel adds an
//! accessory view with a Format pop-up. Picking a format swaps the name's extension, and typing
//! the extension of another format selects it, the way Photoshop's dialog keeps the two in step.
//!
//! No `unsafe` (the workspace forbids it): a pop-up's target/action and notification observers
//! need it, so a short main-queue poll (every 100 ms while the panel is up) reads the pop-up and
//! name field instead, and the Save click applies the same rule once more ([`settle`]) so the
//! saved name matches the pop-up even inside that 100 ms. The panel accepts every format's
//! extension (the picked one first). All AppKit calls are safe bindings; the panel is shown as a
//! sheet with a completion block, so the event loop keeps running (#673).

use std::path::Path;

/// The name with the extension of the format picked (`chosen`, its extensions, first is
/// preferred). A name ending in an extension of any `filters` format (or in none) gets `chosen`'s;
/// any other ending (`scan.2024`) is part of the name and the extension is added after it.
pub(crate) fn retarget_name(name: &str, chosen: &[String], filters: &[(String, Vec<String>)]) -> String {
    let Some(first) = chosen.first() else { return name.to_string() };
    let path = Path::new(name);
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    match ext {
        Some(ext) if chosen.contains(&ext) => name.to_string(),
        Some(ext) if filters.iter().any(|(_, exts)| exts.contains(&ext)) => {
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            format!("{stem}.{first}")
        }
        _ if name.is_empty() => String::new(),
        _ => format!("{name}.{first}"),
    }
}

/// The format a typed name asks for: the index in `filters` of the one owning its extension.
pub(crate) fn format_for_name(name: &str, filters: &[(String, Vec<String>)]) -> Option<usize> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    filters.iter().position(|(_, exts)| exts.contains(&ext))
}

/// Where the pop-up and the name end up once both are read: `seen` is the (format index, name)
/// last put in step, `now` what the panel shows. A pop-up change wins and swaps the name's
/// extension; otherwise a typed extension of another format selects it. The live poll and the
/// Save click both go through this, so the saved name always matches the pop-up (#6490).
pub(crate) fn settle(seen: (usize, &str), now: (usize, &str), filters: &[(String, Vec<String>)]) -> (usize, String) {
    let (index, name) = now;
    if index != seen.0 {
        let renamed = filters.get(index).map(|(_, exts)| retarget_name(name, exts, filters));
        return (index, renamed.unwrap_or_else(|| name.to_string()));
    }
    if name != seen.1 {
        return (format_for_name(name, filters).unwrap_or(index), name.to_string());
    }
    (index, name.to_string())
}

/// The extensions the panel accepts with format `index` picked: that format's first (AppKit adds
/// the first to a name with none), then every other format's, so a typed `.png` is never refused
/// or given a second extension while the pop-up still says PSD.
pub(crate) fn allowed_extensions(index: usize, filters: &[(String, Vec<String>)]) -> Vec<String> {
    let mut v: Vec<String> = filters.get(index).map(|(_, exts)| exts.clone()).unwrap_or_default();
    for (_, exts) in filters {
        for e in exts {
            if !v.contains(e) {
                v.push(e.clone());
            }
        }
    }
    v
}

#[cfg(target_os = "macos")]
pub use mac::save;

#[cfg(target_os = "macos")]
mod mac {
    use super::{allowed_extensions, settle};
    use dispatch2::{DispatchQueue, DispatchTime, MainThreadBound};
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSApplication, NSModalResponse, NSModalResponseOK, NSPopUpButton, NSSavePanel, NSTextField, NSView};
    use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSURL};
    use std::cell::RefCell;
    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    type Filters = Vec<(String, Vec<String>)>;

    /// The panel's answer, handed from the completion block to the dialog thread.
    #[derive(Default)]
    struct Slot {
        done: bool,
        path: Option<String>,
        waker: Option<Waker>,
    }

    struct Answer(Arc<Mutex<Slot>>);

    impl Future for Answer {
        type Output = Option<String>;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<String>> {
            let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.done {
                return Poll::Ready(slot.path.take());
            }
            slot.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn finish(slot: &Mutex<Slot>, path: Option<String>) {
        let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
        slot.done = true;
        slot.path = path;
        if let Some(w) = slot.waker.take() {
            w.wake();
        }
    }

    fn ns_exts(exts: &[String]) -> Retained<NSArray<NSString>> {
        let v: Vec<Retained<NSString>> = exts.iter().map(|e| NSString::from_str(e)).collect();
        NSArray::from_retained_slice(&v)
    }

    #[allow(deprecated)] // allowedContentTypes needs UTType bindings; file types still work on 11+.
    fn allow(panel: &NSSavePanel, index: usize, filters: &Filters) {
        panel.setAllowedFileTypes(Some(&ns_exts(&allowed_extensions(index, filters))));
    }

    /// The Format label and pop-up, centred in a strip the panel shows under the file browser.
    fn accessory(mtm: MainThreadMarker, filters: &Filters) -> (Retained<NSView>, Retained<NSPopUpButton>) {
        const WIDTH: f64 = 460.0;
        const HEIGHT: f64 = 44.0;
        const POPUP_W: f64 = 240.0;
        let view = NSView::initWithFrame(mtm.alloc(), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)));
        let label = NSTextField::labelWithString(&NSString::from_str(&format!("{}:", photocraft_ui_egui::i18n::t("Format"))), mtm);
        label.sizeToFit();
        let label_size = label.frame().size;
        let popup = NSPopUpButton::initWithFrame_pullsDown(mtm.alloc(), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(POPUP_W, 26.0)), false);
        let titles: Vec<Retained<NSString>> = filters.iter().map(|(name, _)| NSString::from_str(name)).collect();
        popup.addItemsWithTitles(&NSArray::from_retained_slice(&titles));
        popup.selectItemAtIndex(0);
        let x = ((WIDTH - (label_size.width + 8.0 + POPUP_W)) / 2.0).max(0.0);
        label.setFrameOrigin(NSPoint::new(x, (HEIGHT - label_size.height) / 2.0));
        popup.setFrameOrigin(NSPoint::new(x + label_size.width + 8.0, (HEIGHT - 26.0) / 2.0));
        view.addSubview(&label);
        view.addSubview(&popup);
        (view, popup)
    }

    /// Keeps the pop-up and the name's extension in step until the panel closes (see module docs).
    struct Keeper {
        panel: Retained<NSSavePanel>,
        popup: Retained<NSPopUpButton>,
        filters: Filters,
        index: usize,
        name: String,
    }

    impl Keeper {
        fn popup_index(&self) -> usize {
            usize::try_from(self.popup.indexOfSelectedItem()).unwrap_or(self.index)
        }

        /// Applies [`settle`] to what the panel shows now.
        fn tick(&mut self) {
            let shown = self.panel.nameFieldStringValue().to_string();
            let (index, name) = settle((self.index, &self.name), (self.popup_index(), &shown), &self.filters);
            if index != self.index {
                if let Ok(at) = isize::try_from(index) {
                    self.popup.selectItemAtIndex(at);
                }
                allow(&self.panel, index, &self.filters);
            }
            if name != shown {
                self.panel.setNameFieldStringValue(&NSString::from_str(&name));
            }
            (self.index, self.name) = (index, name);
        }

        /// The saved file's name in step with the pop-up, for a Save clicked before the poll
        /// saw the last change (the same rule as [`Keeper::tick`], on the name AppKit returned).
        fn finish_name(&self, saved: &str) -> String {
            settle((self.index, &self.name), (self.popup_index(), saved), &self.filters).1
        }
    }

    fn schedule(keeper: MainThreadBound<Rc<RefCell<Keeper>>>, slot: Arc<Mutex<Slot>>) {
        let Ok(when) = DispatchTime::try_from(Duration::from_millis(100)) else { return };
        let _ = DispatchQueue::main().after(when, move || {
            if slot.lock().unwrap_or_else(PoisonError::into_inner).done {
                return;
            }
            let Some(mtm) = MainThreadMarker::new() else { return };
            if let Ok(mut k) = keeper.get(mtm).try_borrow_mut() {
                k.tick();
            }
            schedule(keeper, slot);
        });
    }

    /// Shows the save panel (as a sheet on the main window) and resolves to the chosen path, or
    /// `None` on Cancel. Must be called on the main thread; elsewhere it answers `None` at once.
    pub fn save(filters: Filters, name: Option<String>, dir: Option<std::path::PathBuf>) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> {
        let slot = Arc::new(Mutex::new(Slot::default()));
        let answer = Answer(slot.clone());
        let Some(mtm) = MainThreadMarker::new() else {
            log::error!("save panel asked for off the main thread");
            finish(&slot, None);
            return Box::pin(answer);
        };
        let panel = NSSavePanel::savePanel(mtm);
        panel.setCanCreateDirectories(true);
        panel.setExtensionHidden(false);
        panel.setCanSelectHiddenExtension(false);
        allow(&panel, 0, &filters);
        if let Some(dir) = &dir {
            panel.setDirectoryURL(Some(&NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&dir.to_string_lossy()), true)));
        }
        let start = name.unwrap_or_default();
        panel.setNameFieldStringValue(&NSString::from_str(&start));
        let keeper = (!filters.is_empty()).then(|| {
            let (view, popup) = accessory(mtm, &filters);
            panel.setAccessoryView(Some(&view));
            let keeper = Rc::new(RefCell::new(Keeper { panel: panel.clone(), popup, filters, index: 0, name: start }));
            schedule(MainThreadBound::new(keeper.clone(), mtm), slot.clone());
            keeper
        });
        let done = slot.clone();
        let shown = panel.clone();
        let handler = block2::RcBlock::new(move |response: NSModalResponse| {
            let path = (response == NSModalResponseOK).then(|| shown.URL()).flatten().and_then(|url| url.path()).map(|p| p.to_string());
            let path = path.map(|p| {
                let path = Path::new(&p);
                let (Some(k), Some(saved)) = (keeper.as_ref().and_then(|k| k.try_borrow().ok()), path.file_name().and_then(|n| n.to_str())) else {
                    return p.clone();
                };
                path.with_file_name(k.finish_name(saved)).to_string_lossy().into_owned()
            });
            finish(&done, path);
        });
        // The document window, not a floating panel that happens to be key.
        match NSApplication::sharedApplication(mtm).mainWindow().or_else(|| NSApplication::sharedApplication(mtm).keyWindow()) {
            Some(window) => panel.beginSheetModalForWindow_completionHandler(&window, &handler),
            None => panel.beginWithCompletionHandler(&handler),
        }
        Box::pin(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters() -> Vec<(String, Vec<String>)> {
        [("PSD Document", &["psd", "psb"][..]), ("PDF", &["pdf"]), ("PNG", &["png"]), ("FITS", &["fits", "fit"])]
            .iter()
            .map(|(n, e)| (n.to_string(), e.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    fn exts(e: &[&str]) -> Vec<String> {
        e.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn picking_a_format_swaps_a_known_extension() {
        let f = filters();
        assert_eq!(retarget_name("Front copy.psd", &exts(&["pdf"]), &f), "Front copy.pdf");
        assert_eq!(retarget_name("Front copy.PNG", &exts(&["pdf"]), &f), "Front copy.pdf");
        assert_eq!(retarget_name("Front copy.pdf", &exts(&["psd", "psb"]), &f), "Front copy.psd");
    }

    #[test]
    fn picking_a_format_keeps_its_own_other_extension() {
        assert_eq!(retarget_name("big.psb", &exts(&["psd", "psb"]), &filters()), "big.psb");
        assert_eq!(retarget_name("sky.fit", &exts(&["fits", "fit"]), &filters()), "sky.fit");
    }

    #[test]
    fn picking_a_format_adds_its_extension_after_a_foreign_ending() {
        let f = filters();
        assert_eq!(retarget_name("Front copy", &exts(&["pdf"]), &f), "Front copy.pdf");
        assert_eq!(retarget_name("scan.2024", &exts(&["png"]), &f), "scan.2024.png");
        assert_eq!(retarget_name("", &exts(&["png"]), &f), "");
        assert_eq!(retarget_name("art.psd", &[], &f), "art.psd");
    }

    #[test]
    fn a_pop_up_change_wins_and_swaps_the_extension() {
        let f = filters();
        assert_eq!(settle((0, "Front.psd"), (1, "Front.psd"), &f), (1, "Front.pdf".into()));
        // Saved before the poll saw the pick: the returned name is brought in step too.
        assert_eq!(settle((0, "Front.psd"), (2, "Front.psd"), &f), (2, "Front.png".into()));
    }

    #[test]
    fn a_typed_extension_selects_its_format() {
        let f = filters();
        assert_eq!(settle((0, "x.psd"), (0, "x.png"), &f), (2, "x.png".into()));
        assert_eq!(settle((0, "x.psd"), (0, "x.2024"), &f), (0, "x.2024".into()));
        assert_eq!(settle((1, "x.pdf"), (1, "x.pdf"), &f), (1, "x.pdf".into()));
        assert_eq!(settle((9, "x"), (9, "x"), &f), (9, "x".into()));
    }

    #[test]
    fn the_panel_accepts_every_format_with_the_picked_one_first() {
        let f = filters();
        assert_eq!(allowed_extensions(1, &f), exts(&["pdf", "psd", "psb", "png", "fits", "fit"]));
        assert_eq!(allowed_extensions(0, &f), exts(&["psd", "psb", "pdf", "png", "fits", "fit"]));
        assert_eq!(allowed_extensions(99, &f), exts(&["psd", "psb", "pdf", "png", "fits", "fit"]));
    }

    #[test]
    fn typing_an_extension_picks_its_format() {
        let f = filters();
        assert_eq!(format_for_name("Front copy.pdf", &f), Some(1));
        assert_eq!(format_for_name("Front copy.PNG", &f), Some(2));
        assert_eq!(format_for_name("big.psb", &f), Some(0));
        assert_eq!(format_for_name("Front copy", &f), None);
        assert_eq!(format_for_name("Front copy.p", &f), None);
        assert_eq!(format_for_name("", &f), None);
    }
}
