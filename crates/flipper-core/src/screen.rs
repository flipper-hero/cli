//! The Flipper's 128x64 monochrome display: frame decoding, PNG/ASCII
//! rendering and the D-pad keys, mirroring `FlipperScreen` in the iOS app.

use crate::pb::gui::InputKey;
use crate::pb::gui::ScreenFrame;
use crate::pb::Main;

pub const WIDTH: usize = 128;
pub const HEIGHT: usize = 64;

/// How the firmware presents the buffer relative to the physical display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Normal,
    Flipped,
    Vertical,
    VerticalFlipped,
}

impl Orientation {
    fn from_proto(value: crate::pb::gui::ScreenOrientation) -> Self {
        use crate::pb::gui::ScreenOrientation as Proto;
        match value {
            Proto::HorizontalFlip => Self::Flipped,
            Proto::Vertical => Self::Vertical,
            Proto::VerticalFlip => Self::VerticalFlipped,
            _ => Self::Normal,
        }
    }

    /// Size as shown to the user, after applying the orientation.
    pub fn display_size(self) -> (usize, usize) {
        match self {
            Self::Normal | Self::Flipped => (WIDTH, HEIGHT),
            Self::Vertical | Self::VerticalFlipped => (HEIGHT, WIDTH),
        }
    }
}

/// One frame of the Flipper display. The firmware sends the raw u8g2 buffer:
/// 8 pages of 128 bytes, each byte a vertical strip of 8 pixels with the
/// least significant bit at the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlipperScreenFrame {
    pub buffer: Vec<u8>,
    pub orientation: Orientation,
}

impl FlipperScreenFrame {
    pub fn from_proto(frame: ScreenFrame) -> Option<Self> {
        if frame.data.len() < WIDTH * HEIGHT / 8 {
            return None;
        }
        let orientation = Orientation::from_proto(frame.orientation());
        Some(Self {
            buffer: frame.data,
            orientation,
        })
    }

    /// Raw buffer pixel, in the framebuffer's own coordinates.
    pub fn raw_pixel(&self, x: usize, y: usize) -> bool {
        if x >= WIDTH || y >= HEIGHT || self.buffer.len() < WIDTH * HEIGHT / 8 {
            return false;
        }
        self.buffer[(y / 8) * WIDTH + x] & (1 << (y % 8)) != 0
    }

    /// Pixel as it appears to the user, honoring the orientation.
    pub fn pixel(&self, x: usize, y: usize) -> bool {
        let (w, h) = (WIDTH, HEIGHT);
        match self.orientation {
            Orientation::Normal => self.raw_pixel(x, y),
            Orientation::Flipped => self.raw_pixel(w - 1 - x, h - 1 - y),
            Orientation::Vertical => self.raw_pixel(w - 1 - y, x),
            Orientation::VerticalFlipped => self.raw_pixel(y, h - 1 - x),
        }
    }

    /// Row-major on/off pixels in display orientation.
    pub fn pixels(&self) -> Vec<bool> {
        let (w, h) = self.orientation.display_size();
        let mut out = vec![false; w * h];
        for (index, pixel) in out.iter_mut().enumerate() {
            *pixel = self.pixel(index % w, index / w);
        }
        out
    }

    /// 1-bit grayscale rows for PNG encoding, display oriented.
    pub fn grayscale(&self) -> (usize, usize, Vec<u8>) {
        let (w, h) = self.orientation.display_size();
        let data = self
            .pixels()
            .into_iter()
            .map(|on| if on { 0 } else { 255 })
            .collect();
        (w, h, data)
    }

    /// Terminal rendering at half resolution using block glyphs, display
    /// oriented. Convenient for agents that cannot view images.
    pub fn ascii(&self) -> String {
        let (w, h) = self.orientation.display_size();
        let pixels = self.pixels();
        let shades = [' ', '░', '▒', '▓', '█'];
        let mut out = String::with_capacity((w / 2 + 1) * (h / 2));
        for by in 0..h / 2 {
            for bx in 0..w / 2 {
                let mut level = 0;
                for dy in 0..2 {
                    for dx in 0..2 {
                        if pixels[(by * 2 + dy) * w + bx * 2 + dx] {
                            level += 1;
                        }
                    }
                }
                out.push(shades[level]);
            }
            out.push('\n');
        }
        out
    }
}

/// Extracts a screen frame from an unsolicited message, if that is what it is.
pub fn frame_from_message(message: &Main) -> Option<FlipperScreenFrame> {
    match message.content.as_ref()? {
        crate::pb::main::Content::GuiScreenFrame(frame) => {
            FlipperScreenFrame::from_proto(frame.clone())
        }
        _ => None,
    }
}

/// The D-pad keys, mirroring `FlipperKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipperKey {
    Up,
    Down,
    Left,
    Right,
    Ok,
    Back,
}

impl FlipperKey {
    pub fn proto(self) -> InputKey {
        match self {
            Self::Up => InputKey::Up,
            Self::Down => InputKey::Down,
            Self::Left => InputKey::Left,
            Self::Right => InputKey::Right,
            Self::Ok => InputKey::Ok,
            Self::Back => InputKey::Back,
        }
    }

    pub fn parse_key(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "ok" => Self::Ok,
            "back" => Self::Back,
            _ => return None,
        })
    }
}
