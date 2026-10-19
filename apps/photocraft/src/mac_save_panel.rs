//! File › Save As on macOS with a Format pop-up under the name field, as Photoshop has (#6490).
//!
//! rfd's macOS save panel only restricts the allowed extensions; it shows no file type control,
//! so PDF, PNG and the rest could be reached only by typing the extension. This panel adds an
//! accessory view with a Format pop-up. Picking a format swaps the name's extension, and typing
//! the extension of another format selects it, the way Photoshop's dialog keeps the two in step.
//!
//! No `unsafe` (the workspace forbids it): a pop-up's target/action and notification observers
//! need it, so a short main-queue poll (every 100 ms while the panel is up) reads the pop-up and
//! name field instead. Both reads and writes are safe AppKit calls; the panel itself is shown as a
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

#[cfg(target_os = "macos")]
pub use mac::save;

#[cfg(target_os = "macos")]
mod mac {
    use super::{format_for_name, retarget_name};
    use dispatch2::{DispatchQueue, DispatchTime, MainThreadBound};
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSApplication, NSModalResponse, NSModalResponseOK, NSPopUpButton, NSSavePanel, NSTextField, NSView};
    use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSURL};
    use std::future::Future;
    use std::pin::Pin;
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
    fn allow(panel: &NSSavePanel, exts: &[String]) {
        panel.setAllowedFileTypes(Some(&ns_exts(exts)));
    }

    /// The Format label and pop-up, centred in a strip the panel shows under the file browser.
    fn accessory(mtm: MainThreadMarker, filters: &Filters) -> (Retained<NSView>, Retained<NSPopUpButton>) {
        const WIDTH: f64 = 460.0;
        const HEIGHT: f64 = 44.0;
        const POPUP_W: f64 = 240.0;
        let view = NSView::initWithFrame(mtm.alloc(), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)));
        let label = NSTextField::labelWithString(&NSString::from_str("Format:"), mtm);
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
        index: isize,
        name: String,
    }

    impl Keeper {
        fn tick(&mut self) {
            let index = self.popup.indexOfSelectedItem();
            let name = self.panel.nameFieldStringValue().to_string();
            if index != self.index {
                if let Some((_, exts)) = usize::try_from(index).ok().and_then(|i| self.filters.get(i)) {
                    allow(&self.panel, exts);
                    let renamed = retarget_name(&name, exts, &self.filters);
                    if renamed != name {
                        self.panel.setNameFieldStringValue(&NSString::from_str(&renamed));
                    }
                    self.name = renamed;
                }
                self.index = index;
            } else if name != self.name {
                let picked = format_for_name(&name, &self.filters).and_then(|i| Some((self.filters.get(i)?, isize::try_from(i).ok()?)));
                if let Some(((_, exts), at)) = picked.filter(|&(_, at)| at != index) {
                    self.popup.selectItemAtIndex(at);
                    allow(&self.panel, exts);
                    self.index = at;
                }
                self.name = name;
            }
        }
    }

    fn schedule(sync: MainThreadBound<Keeper>, slot: Arc<Mutex<Slot>>) {
        let Ok(when) = DispatchTime::try_from(Duration::from_millis(100)) else { return };
        let _ = DispatchQueue::main().after(when, move || {
            if slot.lock().unwrap_or_else(PoisonError::into_inner).done {
                return;
            }
            let Some(mtm) = MainThreadMarker::new() else { return };
            let mut sync = sync;
            sync.get_mut(mtm).tick();
            schedule(sync, slot);
        });
    }

    /// Shows the save panel (as a sheet on the key window) and resolves to the chosen path, or
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
        if let Some((_, exts)) = filters.first() {
            allow(&panel, exts);
        }
        if let Some(dir) = &dir {
            panel.setDirectoryURL(Some(&NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&dir.to_string_lossy()), true)));
        }
        let start = name.unwrap_or_default();
        panel.setNameFieldStringValue(&NSString::from_str(&start));
        if !filters.is_empty() {
            let (view, popup) = accessory(mtm, &filters);
            panel.setAccessoryView(Some(&view));
            let sync = Keeper { panel: panel.clone(), popup, filters, index: 0, name: start };
            schedule(MainThreadBound::new(sync, mtm), slot.clone());
        }
        let done = slot.clone();
        let shown = panel.clone();
        let handler = block2::RcBlock::new(move |response: NSModalResponse| {
            let path = (response == NSModalResponseOK).then(|| shown.URL()).flatten().and_then(|url| url.path()).map(|p| p.to_string());
            finish(&done, path);
        });
        match NSApplication::sharedApplication(mtm).keyWindow().or_else(|| NSApplication::sharedApplication(mtm).mainWindow()) {
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
