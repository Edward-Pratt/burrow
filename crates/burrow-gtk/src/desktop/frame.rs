//! The screen's pixels, as the server last drew them.
//!
//! The server decides every size, and vnc-rs holds it to a screen of at
//! most 8192 pixels a side and about 8.3 million pixels, with every
//! rectangle inside it. [`Frame`] checks each update against its own size
//! again before copying.

use vnc::Rect;

/// Bytes a pixel, as asked of the server: blue, green, red, then unused.
pub(super) const PIXEL: usize = 4;

/// The screen's pixels, kept here and drawn as a texture.
#[derive(Default)]
pub(super) struct Frame {
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) pixels: Vec<u8>,
}

impl Frame {
    pub(super) fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.pixels = vec![0; usize::from(width) * usize::from(height) * PIXEL];
    }

    /// Whether `rect` lies inside the screen.
    fn holds(&self, rect: &Rect) -> bool {
        u32::from(rect.x) + u32::from(rect.width) <= u32::from(self.width)
            && u32::from(rect.y) + u32::from(rect.height) <= u32::from(self.height)
    }

    /// Copies `data`, `rect`'s pixels row by row, into place. An update
    /// that doesn't fit is dropped.
    pub(super) fn put(&mut self, rect: Rect, data: &[u8]) {
        let row = usize::from(rect.width) * PIXEL;
        if !self.holds(&rect) || data.len() != row * usize::from(rect.height) {
            return;
        }
        let stride = usize::from(self.width) * PIXEL;
        for (n, line) in data.chunks_exact(row.max(1)).enumerate() {
            let at = (usize::from(rect.y) + n) * stride + usize::from(rect.x) * PIXEL;
            self.pixels[at..at + row].copy_from_slice(line);
        }
    }

    /// Copies the pixels at `from` to `to`, which may overlap.
    pub(super) fn copy(&mut self, to: Rect, from: Rect) {
        let fits = to.width == from.width && to.height == from.height;
        if !fits || !self.holds(&to) || !self.holds(&from) {
            return;
        }
        let row = usize::from(from.width) * PIXEL;
        let stride = usize::from(self.width) * PIXEL;
        let rows: Vec<u8> = (0..usize::from(from.height))
            .flat_map(|n| {
                let at = (usize::from(from.y) + n) * stride + usize::from(from.x) * PIXEL;
                self.pixels[at..at + row].to_vec()
            })
            .collect();
        self.put(to, &rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn updates_land_in_place_and_ones_that_dont_fit_are_dropped() {
        let mut frame = Frame::default();
        frame.resize(4, 3);
        let red = [0, 0, 255, 0].repeat(4);
        frame.put(rect(1, 1, 2, 2), &red);
        let pixel = |frame: &Frame, x: usize, y: usize| {
            let at = (y * 4 + x) * PIXEL;
            frame.pixels[at..at + PIXEL].to_vec()
        };
        assert_eq!(pixel(&frame, 1, 1), [0, 0, 255, 0]);
        assert_eq!(pixel(&frame, 2, 2), [0, 0, 255, 0]);
        assert_eq!(pixel(&frame, 0, 0), [0; 4]);
        // Past the edge, or the wrong amount of data.
        let before = frame.pixels.clone();
        frame.put(rect(3, 2, 2, 1), &[1; 8]);
        frame.put(rect(0, 0, 2, 2), &[1; 4]);
        frame.put(rect(u16::MAX, 0, 2, 1), &[1; 8]);
        assert_eq!(frame.pixels, before);
    }

    #[test]
    fn copies_may_overlap() {
        let mut frame = Frame::default();
        frame.resize(3, 1);
        frame.put(rect(0, 0, 3, 1), &[1, 1, 1, 0, 2, 2, 2, 0, 3, 3, 3, 0]);
        frame.copy(rect(1, 0, 2, 1), rect(0, 0, 2, 1));
        assert_eq!(frame.pixels, [1, 1, 1, 0, 1, 1, 1, 0, 2, 2, 2, 0]);
        let before = frame.pixels.clone();
        frame.copy(rect(2, 0, 2, 1), rect(0, 0, 2, 1));
        assert_eq!(frame.pixels, before);
    }
}
