use core::fmt;
use limine::framebuffer::Framebuffer;

static FONT: &[u8; 4096] = include_bytes!("font8x16.bin");
const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;

pub struct FbConsole {
    base: *mut u8,
    width: usize,
    height: usize,
    pitch: usize,
    bpp: usize,
    shifts: (u8, u8, u8),
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    fg: u32,
    bg: u32,
}

unsafe impl Send for FbConsole {}

impl FbConsole {
    pub fn new(fb: &Framebuffer) -> Self {
        let mut c = Self {
            base: fb.address() as *mut u8,
            width: fb.width as usize,
            height: fb.height as usize,
            pitch: fb.pitch as usize,
            bpp: fb.bpp as usize,
            shifts: (fb.red_mask_shift, fb.green_mask_shift, fb.blue_mask_shift),
            cols: fb.width as usize / GLYPH_W,
            rows: fb.height as usize / GLYPH_H,
            col: 0,
            row: 0,
            fg: 0,
            bg: 0,
        };
        c.fg = c.rgb(0xD0, 0xD0, 0xD0);
        c.bg = c.rgb(0x0B, 0x0E, 0x14);
        c.clear();
        c
    }

    pub fn rgb(&self, r: u8, g: u8, b: u8) -> u32 {
        ((r as u32) << self.shifts.0) | ((g as u32) << self.shifts.1) | ((b as u32) << self.shifts.2)
    }

    pub fn set_fg(&mut self, r: u8, g: u8, b: u8) {
        self.fg = self.rgb(r, g, b);
    }

    #[inline]
    fn put_pixel(&mut self, x: usize, y: usize, color: u32) {
        let off = y * self.pitch + x * (self.bpp / 8);
        unsafe { self.base.add(off).cast::<u32>().write_volatile(color) };
    }

    pub fn clear(&mut self) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, self.bg);
            }
        }
        self.col = 0;
        self.row = 0;
    }

    fn draw_glyph(&mut self, ch: u8, col: usize, row: usize) {
        let glyph = &FONT[ch as usize * GLYPH_H..(ch as usize + 1) * GLYPH_H];
        let x0 = col * GLYPH_W;
        let y0 = row * GLYPH_H;
        for (dy, bits) in glyph.iter().enumerate() {
            for dx in 0..GLYPH_W {
                let on = bits & (0x80 >> dx) != 0;
                let color = if on { self.fg } else { self.bg };
                self.put_pixel(x0 + dx, y0 + dy, color);
            }
        }
    }

    fn scroll(&mut self) {
        let row_bytes = GLYPH_H * self.pitch;
        let total = self.rows * row_bytes;
        unsafe {
            core::ptr::copy(self.base.add(row_bytes), self.base, total - row_bytes);
        }
        let last = self.rows - 1;
        for c in 0..self.cols {
            self.draw_glyph(b' ', c, last);
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 >= self.rows {
            self.scroll();
        } else {
            self.row += 1;
        }
    }

    pub fn put_char(&mut self, ch: u8) {
        match ch {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            b'\t' => {
                let next = (self.col + 4) & !3;
                while self.col < next.min(self.cols) {
                    self.put_char(b' ');
                }
            }
            0x08 => {
                if self.col > 0 {
                    self.col -= 1;
                    self.draw_glyph(b' ', self.col, self.row);
                }
            }
            _ => {
                if self.col >= self.cols {
                    self.newline();
                }
                let g = if (0x20..0x7F).contains(&ch) { ch } else { b'?' };
                self.draw_glyph(g, self.col, self.row);
                self.col += 1;
            }
        }
    }
}

impl fmt::Write for FbConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            self.put_char(b);
        }
        Ok(())
    }
}
