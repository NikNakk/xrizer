// Copyright 2026, Nick Kennedy
// SPDX-License-Identifier: GPL-3.0-or-later
//! Optional API wall-clock diagnostics. Not GPU execution or presentation time.
use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    sync::{Mutex, OnceLock},
    time::Instant,
};
struct Capture {
    epoch: Instant,
    writer: Mutex<Option<BufWriter<File>>>,
}
static CAPTURE: OnceLock<Option<Capture>> = OnceLock::new();
pub struct Span {
    capture: Option<&'static Capture>,
    started: Instant,
    event: &'static str,
    frame: u32,
    eye: i32,
}
impl Span {
    pub fn new(event: &'static str, frame: u32, eye: i32) -> Option<Self> {
        let capture = CAPTURE
            .get_or_init(|| {
                let path = std::env::var_os("XRIZER_API_TRACE")?;
                if path.is_empty() {
                    return None;
                }
                let open = || -> std::io::Result<BufWriter<File>> {
                    let file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)?;
                    let mut writer = BufWriter::with_capacity(16384, file);
                    writeln!(writer, "event,frame,eye,start_ns,duration_ns")?;
                    writer.flush()?;
                    Ok(writer)
                };
                match open() {
                    Ok(writer) => Some(Capture {
                        epoch: Instant::now(),
                        writer: Mutex::new(Some(writer)),
                    }),
                    Err(error) => {
                        eprintln!("[xrizer-api-trace] cannot create {path:?}: {error}");
                        None
                    }
                }
            })
            .as_ref()?;
        Some(Self {
            capture: Some(capture),
            started: Instant::now(),
            event,
            frame,
            eye,
        })
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        let finished = Instant::now();
        let Some(capture) = self.capture else {
            return;
        };
        let Ok(mut guard) = capture.writer.lock() else {
            return;
        };
        let Some(writer) = guard.as_mut() else {
            return;
        };
        let result = writeln!(
            writer,
            "{},{},{},{},{}",
            self.event,
            self.frame,
            self.eye,
            self.started.duration_since(capture.epoch).as_nanos(),
            finished.duration_since(self.started).as_nanos()
        );
        // Flush at handoff so completed frames survive a bounded launcher stop.
        // The measured interval ends before this writer lock or I/O.
        let result = result.and_then(|()| {
            if self.event == "handoff" {
                writer.flush()
            } else {
                Ok(())
            }
        });
        if let Err(error) = result {
            eprintln!("[xrizer-api-trace] capture disabled after write error: {error}");
            *guard = None;
        }
    }
}
