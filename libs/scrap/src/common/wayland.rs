use crate::{
    wayland::{capturable::*, *},
    Frame, TraitCapturer,
};
use std::{io, sync::RwLock, time::Duration};

use super::x11::PixelBuffer;

pub struct Capturer(Display, Box<dyn Recorder>, Vec<u8>);

lazy_static::lazy_static! {
    static ref MAP_ERR: RwLock<Option<fn(err: String)-> io::Error>> = Default::default();
}

pub fn set_map_err(f: fn(err: String) -> io::Error) {
    *MAP_ERR.write().unwrap() = Some(f);
}

fn map_err<E: ToString>(err: E) -> io::Error {
    if let Some(f) = *MAP_ERR.read().unwrap() {
        f(err.to_string())
    } else {
        io::Error::new(io::ErrorKind::Other, err.to_string())
    }
}

// A display recorded through Mutter that changed, or is busy, is no fault of the portal or the
// platform, which is all `MAP_ERR` makes out of an error: the caller builds its capturer again.
// Its stream busy is `TimedOut`, to be tried again soon.
fn map_err_of(display: &Display, err: Box<dyn std::error::Error>) -> io::Error {
    if display.0.held.is_some() {
        let kind = if err.downcast_ref::<crate::mutter_model::Busy>().is_some() {
            io::ErrorKind::TimedOut
        } else {
            io::ErrorKind::Other
        };
        io::Error::new(kind, err.to_string())
    } else {
        map_err(err)
    }
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        let r = display
            .0
            .recorder(false)
            .map_err(|err| map_err_of(&display, err))?;
        Ok(Capturer(display, r, Default::default()))
    }

    pub fn width(&self) -> usize {
        self.0.width()
    }

    pub fn height(&self) -> usize {
        self.0.height()
    }
}

impl TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, timeout: Duration) -> io::Result<Frame<'a>> {
        let display = &self.0;
        match self
            .1
            .capture(timeout.as_millis() as _)
            .map_err(|err| map_err_of(display, err))?
        {
            PixelProvider::BGR0(w, h, x) => Ok(Frame::PixelBuffer(PixelBuffer::new(
                x,
                crate::Pixfmt::BGRA,
                w,
                h,
            ))),
            PixelProvider::RGB0(w, h, x) => Ok(Frame::PixelBuffer(PixelBuffer::new(
                x,
                crate::Pixfmt::RGBA,
                w,
                h,
            ))),
            PixelProvider::NONE => Err(std::io::ErrorKind::WouldBlock.into()),
            _ => Err(map_err("Invalid data")),
        }
    }
}

pub struct Display(pub(crate) pipewire::PipeWireCapturable);

impl Display {
    pub fn primary() -> io::Result<Display> {
        let mut all = Display::all()?;
        if all.is_empty() {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(all.remove(0))
    }

    pub fn all() -> io::Result<Vec<Display>> {
        Ok(pipewire::get_capturables()
            .map_err(map_err)?
            .drain(..)
            .map(|x| Display(x))
            .collect())
    }

    pub fn width(&self) -> usize {
        self.physical_width()
    }

    pub fn height(&self) -> usize {
        self.physical_height()
    }

    pub fn physical_width(&self) -> usize {
        self.0.physical_size.0
    }

    pub fn physical_height(&self) -> usize {
        self.0.physical_size.1
    }

    pub fn logical_width(&self) -> usize {
        self.0.logical_size.0
    }

    pub fn logical_height(&self) -> usize {
        self.0.logical_size.1
    }

    pub fn scale(&self) -> f64 {
        if self.logical_width() == 0 {
            1.0
        } else {
            self.physical_width() as f64 / self.logical_width() as f64
        }
    }

    pub fn origin(&self) -> (i32, i32) {
        self.0.position
    }

    pub fn is_online(&self) -> bool {
        true
    }

    pub fn is_primary(&self) -> bool {
        self.0.primary
    }

    pub fn name(&self) -> String {
        self.0.connector.clone()
    }
}
