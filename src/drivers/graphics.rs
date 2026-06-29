use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
    collections::HashMap,
    io::{stdout, Write},
    time::Duration,
};

use crate::globals::in_tmux;
use crate::RECEIVER_GR;

/* Write one complete Kitty graphics escape sequence, wrapping it in tmux's DCS
 * passthrough envelope (`\ePtmux; ... \e\\`, with inner ESC bytes doubled) when
 * running inside tmux. tmux otherwise swallows the Kitty graphics protocol;
 * passthrough forwards the sequence to the outer terminal. Requires
 * `set -g allow-passthrough on`. The payload is written in spans so the per-byte
 * scan never turns into per-byte writes. */
fn write_graphics_seq(handle: &mut impl Write, seq: &[u8]) -> std::io::Result<()> {
    if !in_tmux() {
        return handle.write_all(seq);
    }
    handle.write_all(b"\x1BPtmux;")?;
    let mut start = 0;
    for (i, &b) in seq.iter().enumerate() {
        if b == 0x1B {
            handle.write_all(&seq[start..i])?;
            handle.write_all(b"\x1B\x1B")?;
            start = i + 1;
        }
    }
    handle.write_all(&seq[start..])?;
    handle.write_all(b"\x1B\\")
}

/* Should be executed only after uncooking the terminal. This method expects the
 * terminal that a non-blocking and unbuffered read from stdin is possible */
pub fn terminal_graphics_test_support() -> Result<(), String> {
    /* Inside tmux the graphics-query reply does not reliably route back to the
     * application, so the probe would always time out. We know the outer terminal
     * supports the protocol (graphics are wrapped for passthrough), so skip it. */
    if in_tmux() {
        return Ok(());
    }

    let mut handle1 = stdout().lock();
    handle1
        .write_all(b"\x1B_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1B\\")
        .unwrap();
    handle1.flush().unwrap();

    /* Timeout since we don't really know yet if the kitty graphics protocol
     * is supported or not */
    let response = RECEIVER_GR
        .get()
        .unwrap()
        .lock()
        .unwrap()
        .recv_timeout(Duration::from_millis(1000))
        .map_err(|x| {
            format!("Could not receive from Graphics Response channel: {}", x)
        })?;

    if !response.payload().contains("OK") {
        Err(format!(
            "Terminal responded with failed graphics response: {}",
            response.payload()
        ))?;
    }
    Ok(())
}

#[allow(dead_code)]
pub fn terminal_graphics_deallocate_id(id: usize) -> Result<(), String> {
    let mut handle = stdout().lock();
    let mut seq: Vec<u8> = Vec::new();
    write!(seq, "\x1B_Ga=d,d=I,i={};\x1B\\", id).unwrap();
    write_graphics_seq(&mut handle, &seq).unwrap();

    handle.flush().unwrap();

    Ok(())
}

pub fn terminal_graphics_transfer_bitmap(
    id: usize,
    width: usize,
    height: usize,
    data: &[u8],
    alpha: bool,
) -> Result<(), String> {
    /* Encode the raw bitmap as PNG and transmit it directly (t=d, f=100), inline
     * over the pty in chunks of up to 4096 base64 bytes, per the Kitty graphics
     * protocol.
     *
     * Two reasons for direct PNG transmission instead of the upstream temp-file
     * medium (t=t):
     *   1. Temp files reference a path that is only valid on the local machine,
     *      so they break over SSH (the remote terminal cannot read or delete the
     *      path, which also hangs the upstream busy-wait loop forever).
     *   2. Raw RGBA pixels are huge -- a single page is several megabytes -- which
     *      saturates the SSH pty and starves input handling. PNG compresses a
     *      document page by an order of magnitude, so it stays responsive over the
     *      wire. `Compression::Fast` keeps encoding latency low on the host. */
    let color = if alpha {
        png::ColorType::Rgba
    } else {
        png::ColorType::Rgb
    };

    let t_start = std::time::Instant::now();
    let mut png_buf: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_buf, width as u32, height as u32);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG header error: {}", e))?;
        writer
            .write_image_data(data)
            .map_err(|e| format!("PNG encode error: {}", e))?;
    }
    let t_encoded = t_start.elapsed();

    /* Dimensions are carried inside the PNG, so s/v are omitted (f=100). */
    let encoded = STANDARD.encode(&png_buf);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();
    let n = chunks.len();

    /* Defer the (slow) write until the user has been briefly idle. The chunked
     * write holds the shared stdout lock for hundreds of milliseconds over SSH;
     * doing that during active scrolling blocks the main loop's per-frame redraw
     * (the page-break stall). By taking the stdout lock only after input quiets
     * down, scrolling of already-transmitted pages stays smooth and freshly
     * rendered pages fill in as soon as scrolling pauses. */
    while crate::globals::idle_ms() < 100
        && crate::globals::RUNNING.load(std::sync::atomic::Ordering::Acquire)
    {
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
    let t_waited = t_start.elapsed() - t_encoded;

    let mut handle = stdout().lock();

    for (idx, chunk) in chunks.iter().enumerate() {
        let more = if idx + 1 < n { 1 } else { 0 };

        /* Build the full escape sequence for this chunk, then write it (wrapped
         * for tmux passthrough when needed). Each chunk must be wrapped as its
         * own complete sequence. */
        let mut seq: Vec<u8> = Vec::with_capacity(chunk.len() + 48);
        if idx == 0 {
            if n == 1 {
                /* Single chunk: omit the m key entirely */
                write!(seq, "\x1B_Gq=2,f=100,i={},t=d;", id).unwrap();
            } else {
                write!(seq, "\x1B_Gq=2,f=100,i={},t=d,m=1;", id).unwrap();
            }
        } else {
            write!(seq, "\x1B_Gm={};", more).unwrap();
        }
        seq.extend_from_slice(chunk);
        seq.extend_from_slice(b"\x1B\\");

        write_graphics_seq(&mut handle, &seq).unwrap();
    }

    handle.flush().unwrap();
    let t_total = t_start.elapsed();

    crate::globals::dlog(&format!(
        "TRANSFER id={} raw={}KB png={}KB b64={}KB encode={}ms waited={}ms write={}ms",
        id,
        data.len() / 1024,
        png_buf.len() / 1024,
        encoded.len() / 1024,
        t_encoded.as_millis(),
        t_waited.as_millis(),
        (t_total - t_encoded - t_waited).as_millis(),
    ));

    Ok(())
}

pub fn terminal_graphics_display_image(
    id: usize,
    col: usize,
    row: usize,

    rect: (usize, usize, usize, usize),

    c: usize,
    r: usize,
) -> Result<(), String> {
    let mut handle = stdout().lock();

    /* Build the placement as one sequence: save cursor, move to the target cell,
     * put the image at the cursor (C=1 keeps the cursor), restore.
     *
     * The whole sequence -- including the cursor move -- must go through tmux
     * passthrough. Otherwise the cursor move is consumed by tmux while the image
     * put reaches the OUTER terminal, which then places the image at its own
     * (unsynced) cursor position -- the misalignment. Passing the cursor move
     * through too positions the outer terminal's cursor right before the put.
     * (Z-index < -1,073,741,824 keeps images behind colored-background cells.) */
    let mut seq: Vec<u8> = Vec::new();
    write!(seq, "\x1B[s\x1B[{};{}H", row, col).unwrap();
    write!(
        seq,
        "\x1B_Gz=-1073741825,a=p,C=1,i={},x={},y={},w={},h={},c={},r={};\x1B\\",
        id, rect.0, rect.1, rect.2, rect.3, c, r
    )
    .unwrap();
    seq.extend_from_slice(b"\x1B[u");
    write_graphics_seq(&mut handle, &seq).unwrap();

    handle
        .flush()
        .map_err(|x: std::io::Error| format!("Could not flush stdout: {}", x))?;

    Ok(())
}

/* A structure which extracts the Kitty graphics response in a lazy way */
#[derive(Debug, Clone)]
pub struct GraphicsResponse {
    source: String,
    loaded: bool,
    control: HashMap<String, String>,
    payload: String,
}

impl GraphicsResponse {
    pub fn new(source: &[u8]) -> Self {
        let source = std::str::from_utf8(source).unwrap();
        let spl: Vec<&str> = source.split(';').collect();

        Self {
            source: spl.first().unwrap_or(&"").to_string(),
            loaded: false,
            control: HashMap::new(),
            payload: spl.get(1).unwrap_or(&"").to_string(),
        }
    }

    #[allow(dead_code)]
    fn load(&mut self) {
        let spl1 = self.source.split(',');
        for kv in spl1 {
            let spl2: Vec<&str> = kv.split('=').collect();
            if spl2.len() != 2 {
                continue;
            }

            let _ = self
                .control
                .insert(spl2[0].to_string(), spl2[1].to_string());
        }

        self.loaded = true;
    }

    #[allow(dead_code)]
    pub fn control(&mut self) -> &HashMap<String, String> {
        if !self.loaded {
            self.load();
        }
        &self.control
    }

    pub fn payload(&self) -> &str {
        self.payload.as_str()
    }
}
