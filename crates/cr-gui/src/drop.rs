//! Things dropped on the window: files, and images dragged from a browser.
//!
//! On Windows the window's own drop handling only takes real files, so a browser image (which
//! Chrome, Edge and Firefox offer as a "virtual file" with its link, not a file on disk) is
//! refused with a "no" cursor. CleanGroove registers its own drop target instead (`windows`
//! module below), which takes real files, the virtual file's bytes, and failing those the
//! image's web address. Elsewhere egui's dropped files are used.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// One thing dropped on the window.
#[derive(Debug, Clone, PartialEq)]
pub enum DropItem {
    File(PathBuf),
    /// Data without a file on disk (a browser's dragged image), with its name.
    Bytes { name: String, bytes: Vec<u8> },
    /// A web address (a dragged link or image whose data was not offered).
    Url(String),
}

/// Where drops wait for the next frame.
#[derive(Default)]
pub struct DropInbox {
    hovering: AtomicBool,
    items: Mutex<Vec<DropItem>>,
}

impl DropInbox {
    /// True while something is being dragged over the window.
    pub fn hovering(&self) -> bool {
        self.hovering.load(Ordering::Relaxed)
    }

    pub fn take(&self) -> Vec<DropItem> {
        std::mem::take(&mut *self.items.lock().unwrap())
    }

    fn set_hovering(&self, on: bool) {
        self.hovering.store(on, Ordering::Relaxed);
    }

    fn push(&self, items: Vec<DropItem>) {
        self.items.lock().unwrap().extend(items);
    }
}

impl DropItem {
    /// From egui's dropped files (the other platforms).
    pub fn from_egui(f: &egui::DroppedFile) -> Option<DropItem> {
        if let Some(b) = &f.bytes {
            return Some(DropItem::Bytes { name: f.name.clone(), bytes: b.to_vec() });
        }
        f.path.clone().map(DropItem::File)
    }
}

/// The first http(s) address in dropped link text ("UniformResourceLocator" data is the
/// address; a Firefox "x-moz-url" is the address, a line break and the title).
pub fn url_from_text(text: &str) -> Option<String> {
    text.lines()
        .map(|l| l.trim().trim_end_matches('\0'))
        .find(|l| l.starts_with("http://") || l.starts_with("https://"))
        .map(String::from)
}

#[cfg(windows)]
pub use self::windows_target::register;

#[cfg(windows)]
mod windows_target {
    use super::{url_from_text, DropInbox, DropItem};
    use std::path::PathBuf;
    use std::sync::Arc;

    use windows::core::{implement, w, PCWSTR};
    use windows::Win32::Foundation::{HWND, POINTL};
    use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL, TYMED_ISTREAM};
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{
        IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium, CF_HDROP, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY,
        DROPEFFECT_NONE,
    };
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
    use windows::Win32::UI::Shell::{DragQueryFileW, FILEDESCRIPTORW, FILEGROUPDESCRIPTORW, HDROP};

    /// Makes `hwnd` take drops into the returned inbox; `ctx` is asked to repaint on each.
    pub fn register(hwnd: isize, ctx: egui::Context) -> Result<Arc<DropInbox>, String> {
        let inbox = Arc::new(DropInbox::default());
        unsafe {
            // OLE drag and drop needs OLE on this (the window's) thread; already set up is fine.
            let _ = OleInitialize(None);
            let target: IDropTarget = Target { inbox: Arc::clone(&inbox), ctx, accept: Default::default() }.into();
            RegisterDragDrop(HWND(hwnd), &target).map_err(|e| e.to_string())?;
        }
        Ok(inbox)
    }

    #[implement(IDropTarget)]
    struct Target {
        inbox: Arc<DropInbox>,
        ctx: egui::Context,
        accept: std::sync::atomic::AtomicBool,
    }

    fn format(name: PCWSTR) -> u16 {
        unsafe { RegisterClipboardFormatW(name) as u16 }
    }

    fn query(data: &IDataObject, cf: u16, index: i32, tymed: u32) -> FORMATETC {
        let _ = data;
        FORMATETC { cfFormat: cf, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: index, tymed }
    }

    /// The bytes behind a medium (an HGLOBAL or a stream).
    unsafe fn medium_bytes(m: &STGMEDIUM) -> Option<Vec<u8>> {
        if m.tymed == TYMED_HGLOBAL.0 as u32 {
            let h = m.u.hGlobal;
            let p = GlobalLock(h) as *const u8;
            if p.is_null() {
                return None;
            }
            let out = std::slice::from_raw_parts(p, GlobalSize(h)).to_vec();
            let _ = GlobalUnlock(h);
            Some(out)
        } else if m.tymed == TYMED_ISTREAM.0 as u32 {
            let stream = (*m.u.pstm).as_ref()?;
            let mut out = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let mut read = 0u32;
                let hr = stream.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut read));
                if hr.is_err() || read == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..read as usize]);
            }
            Some(out)
        } else {
            None
        }
    }

    unsafe fn get(data: &IDataObject, cf: u16, index: i32) -> Option<Vec<u8>> {
        let fmt = query(data, cf, index, (TYMED_HGLOBAL.0 | TYMED_ISTREAM.0) as u32);
        let mut m = data.GetData(&fmt).ok()?;
        let out = medium_bytes(&m);
        ReleaseStgMedium(&mut m);
        out
    }

    fn utf16(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        String::from_utf16_lossy(&units)
    }

    /// True when the data offers anything CleanGroove can take.
    unsafe fn offers(data: &IDataObject) -> bool {
        let any = (TYMED_HGLOBAL.0 | TYMED_ISTREAM.0) as u32;
        [CF_HDROP.0, format(w!("FileContents")), format(w!("UniformResourceLocatorW")), format(w!("UniformResourceLocator"))]
            .iter()
            .any(|&cf| data.QueryGetData(&query(data, cf, -1, any)).is_ok() || data.QueryGetData(&query(data, cf, 0, any)).is_ok())
    }

    /// Real files first, then the virtual files' bytes, then the link.
    unsafe fn read(data: &IDataObject) -> Vec<DropItem> {
        let mut out = Vec::new();
        let fmt = query(data, CF_HDROP.0, -1, TYMED_HGLOBAL.0 as u32);
        if let Ok(mut m) = data.GetData(&fmt) {
            let hdrop = HDROP(m.u.hGlobal.0 as isize);
            let n = DragQueryFileW(hdrop, u32::MAX, None);
            for i in 0..n {
                let len = DragQueryFileW(hdrop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                DragQueryFileW(hdrop, i, Some(&mut buf));
                out.push(DropItem::File(PathBuf::from(String::from_utf16_lossy(&buf[..len]))));
            }
            ReleaseStgMedium(&mut m);
            if !out.is_empty() {
                return out;
            }
        }
        if let Some(desc) = get(data, format(w!("FileGroupDescriptorW")), -1) {
            if desc.len() >= std::mem::size_of::<FILEGROUPDESCRIPTORW>() {
                // A packed structure in a byte buffer: read with unaligned copies.
                let items = u32::from_le_bytes([desc[0], desc[1], desc[2], desc[3]]) as usize;
                let size = std::mem::size_of::<FILEDESCRIPTORW>();
                let count = items.min((desc.len() - 4) / size);
                let contents = format(w!("FileContents"));
                for i in 0..count {
                    let d: FILEDESCRIPTORW = std::ptr::read_unaligned(desc.as_ptr().add(4 + i * size).cast());
                    let file_name = d.cFileName;
                    let name = String::from_utf16_lossy(&file_name[..file_name.iter().position(|&c| c == 0).unwrap_or(file_name.len())]);
                    if let Some(bytes) = get(data, contents, i as i32).filter(|b| !b.is_empty()) {
                        out.push(DropItem::Bytes { name, bytes });
                    }
                }
            }
        }
        if out.is_empty() {
            let url = get(data, format(w!("UniformResourceLocatorW")), -1)
                .map(|b| utf16(&b))
                .or_else(|| get(data, format(w!("UniformResourceLocator")), -1).map(|b| String::from_utf8_lossy(&b).into_owned()))
                .or_else(|| get(data, CF_UNICODETEXT.0, -1).map(|b| utf16(&b)))
                .and_then(|t| url_from_text(&t));
            out.extend(url.map(DropItem::Url));
        }
        out
    }

    #[allow(non_snake_case)]
    impl IDropTarget_Impl for Target {
        fn DragEnter(&self, pdataobj: Option<&IDataObject>, _keys: MODIFIERKEYS_FLAGS, _pt: &POINTL, pdweffect: *mut DROPEFFECT) -> windows::core::Result<()> {
            let ok = pdataobj.is_some_and(|d| unsafe { offers(d) });
            self.accept.store(ok, std::sync::atomic::Ordering::Relaxed);
            self.inbox.set_hovering(ok);
            self.ctx.request_repaint();
            unsafe { *pdweffect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
            Ok(())
        }

        fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, _pt: &POINTL, pdweffect: *mut DROPEFFECT) -> windows::core::Result<()> {
            let ok = self.accept.load(std::sync::atomic::Ordering::Relaxed);
            unsafe { *pdweffect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
            Ok(())
        }

        fn DragLeave(&self) -> windows::core::Result<()> {
            self.inbox.set_hovering(false);
            self.ctx.request_repaint();
            Ok(())
        }

        fn Drop(&self, pdataobj: Option<&IDataObject>, _keys: MODIFIERKEYS_FLAGS, _pt: &POINTL, pdweffect: *mut DROPEFFECT) -> windows::core::Result<()> {
            self.inbox.set_hovering(false);
            let items = pdataobj.map(|d| unsafe { read(d) }).unwrap_or_default();
            unsafe { *pdweffect = if items.is_empty() { DROPEFFECT_NONE } else { DROPEFFECT_COPY } };
            self.inbox.push(items);
            self.ctx.request_repaint();
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::mem::ManuallyDrop;
        use windows::Win32::System::Com::STGMEDIUM_0;
        use windows::Win32::System::Memory::{GlobalAlloc, GMEM_MOVEABLE};
        use windows::Win32::UI::Shell::SHCreateDataObject;

        unsafe fn put(obj: &IDataObject, cf: u16, index: i32, bytes: &[u8]) {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).unwrap();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), GlobalLock(h) as *mut u8, bytes.len());
            let _ = GlobalUnlock(h);
            let fmt = query(obj, cf, index, TYMED_HGLOBAL.0 as u32);
            let m = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: h }, pUnkForRelease: ManuallyDrop::new(None) };
            obj.SetData(&fmt, &m, true).unwrap();
        }

        fn wide(s: &str) -> Vec<u8> {
            s.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()).collect()
        }

        unsafe fn data_object() -> IDataObject {
            let _ = OleInitialize(None);
            SHCreateDataObject(None, None, None::<&IDataObject>).unwrap()
        }

        /// What Chrome or Edge hands over when an image is dragged out of a page: a virtual
        /// file (its name and bytes) and the image's address, no file on disk.
        #[test]
        fn test_browser_image_drag_gives_its_bytes() {
            unsafe {
                let obj = data_object();
                let mut fd: FILEDESCRIPTORW = std::mem::zeroed();
                for (i, u) in "cover.webp".encode_utf16().enumerate() {
                    fd.cFileName[i] = u;
                }
                let mut group = 1u32.to_le_bytes().to_vec();
                group.extend_from_slice(std::slice::from_raw_parts((&fd as *const FILEDESCRIPTORW).cast::<u8>(), std::mem::size_of::<FILEDESCRIPTORW>()));
                put(&obj, format(w!("FileGroupDescriptorW")), -1, &group);
                put(&obj, format(w!("FileContents")), 0, b"RIFF....WEBPdata");
                put(&obj, format(w!("UniformResourceLocatorW")), -1, &wide("https://example.com/cover.webp"));
                assert!(offers(&obj));
                assert_eq!(read(&obj), vec![DropItem::Bytes { name: "cover.webp".into(), bytes: b"RIFF....WEBPdata".to_vec() }]);
            }
        }

        #[test]
        fn test_link_only_gives_the_address() {
            unsafe {
                let obj = data_object();
                put(&obj, format(w!("UniformResourceLocatorW")), -1, &wide("https://example.com/a.jpg"));
                assert!(offers(&obj));
                assert_eq!(read(&obj), vec![DropItem::Url("https://example.com/a.jpg".into())]);
            }
        }

        #[test]
        fn test_files_from_a_folder() {
            unsafe {
                let obj = data_object();
                // DROPFILES: offset of the list (20), drop point, non-client flag, wide names.
                let mut b = Vec::new();
                for v in [20u32, 0, 0, 0, 1] {
                    b.extend_from_slice(&v.to_le_bytes());
                }
                b.extend(wide(r"C:\Music\cover.jpg"));
                b.extend([0, 0]);
                put(&obj, CF_HDROP.0, -1, &b);
                assert!(offers(&obj));
                assert_eq!(read(&obj), vec![DropItem::File(PathBuf::from(r"C:\Music\cover.jpg"))]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_from_dropped_text() {
        assert_eq!(url_from_text("https://i.discogs.com/a.jpg\0"), Some("https://i.discogs.com/a.jpg".into()));
        assert_eq!(url_from_text("http://x.org/c.png\r\nCover title"), Some("http://x.org/c.png".into()));
        assert_eq!(url_from_text("file:///C:/x.jpg"), None);
    }

    #[test]
    fn test_inbox_hands_over_once() {
        let inbox = DropInbox::default();
        inbox.push(vec![DropItem::Url("https://a/b.jpg".into())]);
        assert_eq!(inbox.take().len(), 1);
        assert!(inbox.take().is_empty());
    }
}
