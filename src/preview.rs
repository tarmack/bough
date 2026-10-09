//! Image preview state, pane rendering, and post-draw kitty sync.
//!
//! The diff pane renders a placeholder header plus blank space while a preview
//! is active; the actual image escape sequences are written by [`sync`] after
//! each `terminal.draw`, because the kitty protocol draws its pixels at the
//! cursor and must not be overwritten by ratatui's buffered cell diffing.

use std::io::{self, Write};

use crossbeam_channel::Receiver;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::Frame;

use crate::app::App;
use crate::kitty;

/// True when `path`'s extension names a decodable image format.
pub fn is_image_path(path: &str) -> bool {
    match crate::highlight::path_ext(path) {
        Some(ext) => matches!(
            ext.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
        ),
        None => false,
    }
}

/// A decoded image ready for the terminal, with its re-encoded PNG bytes and
/// final (possibly downscaled) pixel dimensions.
#[derive(Debug, Clone)]
pub struct PreviewImage {
    pub path: String,
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Preview app-state. `area` is filled in by the view each frame; `placed`
/// tracks what escape sequence is currently on screen so resizes and selection
/// changes re-place or delete the image.
pub struct Preview {
    pub enabled: bool,
    pub requested: Option<String>,
    pub current: Option<PreviewImage>,
    pub area: Option<Rect>,
    placed: Option<(u32, String, Rect, u64)>,
    next_id: u32,
    generation: u64,
    /// Latched `place` failure for `failed_path`: a broken stdout (or similar)
    /// fails every frame, so stop re-encoding that image until the selection
    /// target changes.
    failed: bool,
    failed_path: Option<String>,
}

impl Preview {
    pub fn new() -> Self {
        Preview {
            enabled: false,
            requested: None,
            current: None,
            area: None,
            placed: None,
            next_id: 0,
            generation: 0,
            failed: false,
            failed_path: None,
        }
    }

    pub fn active(&self) -> bool {
        self.enabled && self.current.is_some()
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Accept a decoded preview, but only if it matches the currently-requested
    /// path (stale images from an earlier selection are dropped). A successful
    /// accept bumps `generation` so [`sync`] re-places an image re-decoded after
    /// an edit even when its path and rect are unchanged.
    pub fn accept(&mut self, img: PreviewImage) {
        if self.requested.as_deref() == Some(img.path.as_str()) {
            self.generation = self.generation.wrapping_add(1);
            self.current = Some(img);
        }
    }
}

/// Draw the preview header and blank body for the active image, and record the
/// body rect for [`sync`]. Does not emit any image escape output.
pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let Some(img) = app.preview.current.as_ref() else {
        app.preview.area = None;
        return;
    };

    let name = img.path.rsplit('/').next().unwrap_or(&img.path);
    let kb = img.png.len() as f64 / 1024.0;
    let header = format!("{name}  {}x{}  {kb:.1} KB", img.width, img.height);
    let line = Line::styled(header, Style::default().fg(Color::DarkGray));
    f.buffer_mut().set_line(area.x, area.y, &line, area.width);

    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1),
    );
    if body.height == 0 || body.width == 0 {
        // No room for an image below the header; leave `area` unset so `sync`
        // deletes rather than painting over the header row.
        app.preview.area = None;
        return;
    }
    let buf = f.buffer_mut();
    for y in body.y..body.y + body.height {
        for x in body.x..body.x + body.width {
            buf[(x, y)].set_char(' ').set_style(Style::reset());
        }
    }
    app.preview.area = Some(body);
}

/// Reconcile the on-screen image with the current preview state. Call after
/// every `terminal.draw`. Re-places when the image, its rect, or its generation
/// changed (a resize changes the rect; a re-decode bumps the generation) and
/// deletes when the image goes away.
pub fn sync(w: &mut impl Write, preview: &mut Preview) -> io::Result<()> {
    // A latched failure belongs to one selection target; a new target retries
    // from scratch instead of inheriting the old failure.
    if preview.failed && preview.failed_path.as_deref() != preview.requested.as_deref() {
        preview.failed = false;
        preview.failed_path = None;
    }
    if let (Some(img), Some(area)) = (preview.current.as_ref(), preview.area) {
        if let Some((_, path, rect, gen)) = preview.placed.as_ref() {
            if path == &img.path && *rect == area && *gen == preview.generation {
                return Ok(());
            }
        }
        if let Some((old_id, _, _, _)) = preview.placed.take() {
            if let Err(e) = kitty::delete(w, old_id) {
                preview.placed = None;
                let _ = w.flush();
                return Err(e);
            }
        }
        if preview.failed && preview.failed_path.as_deref() == Some(img.path.as_str()) {
            // This image already failed: skip the re-encode instead of failing
            // every frame. Still flush anything written above (e.g. a delete).
            return w.flush();
        }
        let mut id = preview.next_id.wrapping_add(1);
        if id == 0 {
            id = 1;
        }
        preview.next_id = id;
        if let Err(e) = kitty::place(
            w,
            id,
            &img.png,
            img.width,
            img.height,
            area,
            kitty::cell_size(),
        ) {
            preview.failed = true;
            preview.failed_path = Some(img.path.clone());
            preview.placed = None;
            let _ = w.flush();
            return Err(e);
        }
        preview.placed = Some((id, img.path.clone(), area, preview.generation));
    } else if let Some((old_id, _, _, _)) = preview.placed.take() {
        if let Err(e) = kitty::delete(w, old_id) {
            preview.placed = None;
            let _ = w.flush();
            return Err(e);
        }
    }
    w.flush()
}

/// Delete every image the terminal is holding. Call on exit so previews do not
/// persist onto the primary screen.
pub fn cleanup(w: &mut impl Write) -> io::Result<()> {
    kitty::delete_all(w)?;
    w.flush()
}

/// Drain the preview channel, keeping only the newest image, and return it.
/// The caller applies it via [`Preview::accept`], which rejects stale images.
pub fn drain_previews(rx: &Receiver<PreviewImage>) -> Option<PreviewImage> {
    let mut latest = None;
    while let Ok(img) = rx.try_recv() {
        latest = Some(img);
    }
    latest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_extensions_case_insensitive() {
        assert!(is_image_path("photo.PNG"));
        assert!(is_image_path("a/b/c.jpeg"));
        assert!(is_image_path("dir/img.webp"));
        assert!(is_image_path("x.gif"));
        assert!(is_image_path("x.bmp"));
    }

    #[test]
    fn non_images_rejected() {
        assert!(!is_image_path("README"));
        assert!(!is_image_path("x.txt"));
        assert!(!is_image_path("archive.tar.gz"));
        assert!(!is_image_path("dir/"));
        assert!(!is_image_path(".png"));
    }

    #[test]
    fn active_requires_enabled_and_current() {
        let mut p = Preview::new();
        assert!(!p.active());
        p.enabled = true;
        assert!(!p.active());
        p.current = Some(PreviewImage {
            path: "x.png".into(),
            png: vec![1, 2, 3],
            width: 1,
            height: 1,
        });
        assert!(p.active());
    }

    #[test]
    fn drain_keeps_last() {
        let (tx, rx) = crossbeam_channel::unbounded::<PreviewImage>();
        let mk = |n: u8| PreviewImage {
            path: "x.png".into(),
            png: vec![n],
            width: 1,
            height: 1,
        };
        tx.send(mk(1)).unwrap();
        tx.send(mk(2)).unwrap();
        tx.send(mk(3)).unwrap();
        let got = drain_previews(&rx).unwrap();
        assert_eq!(got.png, vec![3]);
        assert!(drain_previews(&rx).is_none());
    }

    #[test]
    fn accept_only_matching_requested_and_bumps_generation() {
        let mut p = Preview::new();
        p.requested = Some("a.png".into());
        let mk = |path: &str| PreviewImage {
            path: path.into(),
            png: vec![1],
            width: 1,
            height: 1,
        };

        p.accept(mk("other.png"));
        assert!(p.current.is_none());
        assert_eq!(p.generation, 0);

        p.accept(mk("a.png"));
        assert_eq!(p.generation, 1);
        assert_eq!(p.current.as_ref().unwrap().path, "a.png");

        p.accept(mk("a.png"));
        assert_eq!(p.generation, 2);
    }

    #[test]
    fn sync_places_and_deletes() {
        let mut p = Preview::new();
        p.enabled = true;
        p.current = Some(PreviewImage {
            path: "x.png".into(),
            png: b"hello".to_vec(),
            width: 4,
            height: 3,
        });
        p.area = Some(Rect::new(0, 0, 10, 10));

        let mut out = Vec::new();
        sync(&mut out, &mut p).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("a=T,f=100,t=d,C=1,i=1,s=4,v=3"));
        assert!(p.placed.is_some());

        // Same state: no new escape output.
        let mut out2 = Vec::new();
        sync(&mut out2, &mut p).unwrap();
        assert!(out2.is_empty());

        // Image gone: the placed image is deleted.
        p.current = None;
        let mut out3 = Vec::new();
        sync(&mut out3, &mut p).unwrap();
        let s3 = String::from_utf8(out3).unwrap();
        assert!(s3.contains("a=d,d=I,i=1"));
        assert!(p.placed.is_none());
    }

    #[test]
    fn sync_replaces_when_generation_changes() {
        let mut p = Preview::new();
        p.enabled = true;
        p.requested = Some("x.png".into());
        p.accept(PreviewImage {
            path: "x.png".into(),
            png: b"one".to_vec(),
            width: 4,
            height: 3,
        });
        p.area = Some(Rect::new(0, 0, 10, 10));

        let mut out = Vec::new();
        sync(&mut out, &mut p).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("i=1"));

        // Same path/rect, but a re-decoded image bumps the generation.
        p.accept(PreviewImage {
            path: "x.png".into(),
            png: b"two".to_vec(),
            width: 4,
            height: 3,
        });
        let mut out2 = Vec::new();
        sync(&mut out2, &mut p).unwrap();
        let s2 = String::from_utf8(out2).unwrap();
        assert!(s2.contains("a=d,d=I,i=1"));
        assert!(s2.contains("a=T"));
        assert!(s2.contains("i=2"));
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("boom"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sync_clears_placed_on_write_error() {
        let mut p = Preview::new();
        p.placed = Some((1, "x.png".into(), Rect::new(0, 0, 10, 10), 0));
        p.current = None;

        let err = sync(&mut FailingWriter, &mut p).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(p.placed.is_none());
    }

    #[test]
    fn sync_latches_persistent_place_failure() {
        let mut p = Preview::new();
        p.enabled = true;
        p.requested = Some("x.png".into());
        p.current = Some(PreviewImage {
            path: "x.png".into(),
            png: b"hello".to_vec(),
            width: 4,
            height: 3,
        });
        p.area = Some(Rect::new(0, 0, 10, 10));

        // First attempt fails on `place` and latches the failure.
        assert!(sync(&mut FailingWriter, &mut p).is_err());
        assert!(p.failed);
        assert_eq!(p.failed_path.as_deref(), Some("x.png"));
        assert!(p.placed.is_none());

        // Same image + area: the latch skips the re-encode and reports success.
        assert!(sync(&mut FailingWriter, &mut p).is_ok());
        assert!(p.placed.is_none());

        // A changed selection target clears the latch and retries (fails again).
        p.requested = Some("y.png".into());
        p.current = Some(PreviewImage {
            path: "y.png".into(),
            png: b"world".to_vec(),
            width: 4,
            height: 3,
        });
        assert!(sync(&mut FailingWriter, &mut p).is_err());
        assert_eq!(p.failed_path.as_deref(), Some("y.png"));
    }
}
