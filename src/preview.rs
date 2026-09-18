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
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if stem.is_empty() {
        return false;
    }
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
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
    placed: Option<(u32, String, Rect)>,
    next_id: u32,
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
        }
    }

    pub fn active(&self) -> bool {
        self.enabled && self.current.is_some()
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }
}

impl Default for Preview {
    fn default() -> Self {
        Preview::new()
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
    if body.height > 0 && body.width > 0 {
        let blank = " ".repeat(body.width as usize);
        for y in body.y..body.y + body.height {
            f.buffer_mut()
                .set_string(body.x, y, blank.clone(), Style::reset());
        }
    }
    app.preview.area = Some(if body.height > 0 {
        body
    } else {
        Rect::new(area.x, area.y, area.width, 1)
    });
}

/// Reconcile the on-screen image with the current preview state. Call after
/// every `terminal.draw`. Re-places when the image or its rect changed (a
/// resize changes the rect) and deletes when the image goes away.
pub fn sync(w: &mut impl Write, preview: &mut Preview) -> io::Result<()> {
    if let (Some(img), Some(area)) = (preview.current.as_ref(), preview.area) {
        if let Some((_, path, rect)) = preview.placed.as_ref() {
            if path == &img.path && *rect == area {
                return Ok(());
            }
        }
        if let Some((old_id, _, _)) = preview.placed.take() {
            kitty::delete(w, old_id)?;
        }
        let id = preview.next_id.wrapping_add(1);
        preview.next_id = id;
        kitty::place(w, id, &img.png, img.width, img.height, area, kitty::cell_size())?;
        preview.placed = Some((id, img.path.clone(), area));
    } else if let Some((old_id, _, _)) = preview.placed.take() {
        kitty::delete(w, old_id)?;
    }
    w.flush()
}

/// Delete every image the terminal is holding. Call on exit so previews do not
/// persist onto the primary screen.
pub fn cleanup(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"\x1b_Ga=d,d=A\x1b\\")?;
    w.flush()
}

/// Drain the preview channel, keeping only the newest image, and return it.
/// The caller applies it via [`App::apply_preview`], which rejects stale images.
pub fn drain_previews(
    rx: &Receiver<PreviewImage>,
    _preview: &mut Preview,
) -> Option<PreviewImage> {
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
        let mut preview = Preview::new();
        let got = drain_previews(&rx, &mut preview).unwrap();
        assert_eq!(got.png, vec![3]);
        assert!(drain_previews(&rx, &mut preview).is_none());
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
        assert_eq!(out3, b"\x1b_Ga=d,d=I,i=1\x1b\\");
        assert!(p.placed.is_none());
    }
}