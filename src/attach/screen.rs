//! Sandbox-side screen model. The pty-host feeds every byte of agent
//! output through this and, on attach, asks it for a bounded ANSI
//! `snapshot()` that repaints the *current* screen on a fresh terminal —
//! the PTY analogue of replaying the event stream from a sequence number.
//!
//! Backed by `vt100`, whose `state_formatted()` carries the out-of-band
//! modes (mouse tracking + encoding, bracketed paste, application cursor
//! keys, hidden cursor, title) that orca had to hand-mirror on top of
//! xterm's serialize addon. See `docs/attach-transport.md`.

use vt100::Parser;

pub(crate) struct ScreenModel {
    parser: Parser,
}

#[allow(dead_code)] // consumed by the pty-host in phase 2
impl ScreenModel {
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        // scrollback 0: the snapshot is the visible screen; scrollback
        // history is out of scope for repaint-on-attach.
        Self {
            parser: Parser::new(rows, cols, 0),
        }
    }

    /// Feed live PTY output. Must be called for every byte even when no
    /// client is attached, so a later snapshot is always current.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(rows, cols);
    }

    /// ANSI bytes that, written to a fresh terminal of the same size,
    /// reproduce the current screen + modes. Directly consumable by
    /// xterm.js (write it, then write live `Data`).
    pub(crate) fn snapshot(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut out = Vec::new();
        // alt-screen is a buffer *switch*, not grid state — state_formatted()
        // won't emit it, so prepend it from the flag (orca's separate
        // `isAlternateScreen`, baked into the bytes here).
        if screen.alternate_screen() {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        // state_formatted() already writes the grid (contents_formatted) plus
        // the input modes + title — it's a superset, so we must NOT also
        // append contents_formatted() or the whole screen ships twice.
        out.extend_from_slice(&screen.state_formatted());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vt100::Color;

    // A deliberately hard screen: alt-screen, 16/256/RGB color, bold,
    // underline, reverse, cursor parking, plus the out-of-band modes
    // (bracketed paste, mouse tracking + SGR encoding, app-cursor, hidden
    // cursor) and an OSC title. Fed directly — no PTY needed.
    fn hard_screen() -> Vec<u8> {
        let mut b = Vec::new();
        let mut push = |s: &str| b.extend_from_slice(s.as_bytes());
        push("\x1b[?1049h\x1b[2J\x1b[H");
        push("\x1b]0;proto-title\x07");
        push("\x1b[2;5H\x1b[1;31mHELLO\x1b[0m");
        push("\x1b[4;10H\x1b[38;5;208m\x1b[48;5;18mBLOCK\x1b[0m");
        push("\x1b[6;3H\x1b[38;2;0;255;128mTRUECOLOR\x1b[0m");
        push("\x1b[8;3H\x1b[4mUNDER\x1b[0m");
        push("\x1b[9;3H\x1b[7mREVERSE\x1b[0m");
        push("\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1h\x1b[?25l");
        push("\x1b[15;40H");
        b
    }

    fn modes(p: &Parser) -> String {
        let s = p.screen();
        format!(
            "alt={} hide={} appcur={} bpaste={} mouse={:?}/{:?} title={:?}",
            s.alternate_screen(),
            s.hide_cursor(),
            s.application_cursor(),
            s.bracketed_paste(),
            s.mouse_protocol_mode(),
            s.mouse_protocol_encoding(),
            s.title(),
        )
    }

    fn cell(p: &Parser, row: u16, col: u16) -> (String, Color, Color, bool) {
        let c = p.screen().cell(row, col);
        (
            c.map(|c| c.contents()).unwrap_or_default(),
            c.map(|c| c.fgcolor()).unwrap_or(Color::Default),
            c.map(|c| c.bgcolor()).unwrap_or(Color::Default),
            c.map(|c| c.bold()).unwrap_or(false),
        )
    }

    #[test]
    fn snapshot_round_trips_grid_cursor_and_modes() {
        let mut live = ScreenModel::new(80, 24);
        live.feed(&hard_screen());

        // snapshot -> a fresh parser, as a reattaching client's terminal would
        let mut restored = Parser::new(24, 80, 0);
        restored.process(&live.snapshot());

        let lp = &live.parser;
        assert_eq!(
            lp.screen().contents(),
            restored.screen().contents(),
            "screen text must survive the snapshot"
        );
        assert_eq!(
            lp.screen().cursor_position(),
            restored.screen().cursor_position(),
            "parked cursor must survive the snapshot"
        );
        assert_eq!(
            modes(lp),
            modes(&restored),
            "out-of-band modes must survive"
        );
        for (label, r, c) in [("HELLO", 1, 4), ("BLOCK", 3, 9), ("TRUECOLOR", 5, 2)] {
            assert_eq!(
                cell(lp, r, c),
                cell(&restored, r, c),
                "{label} cell attrs must survive"
            );
        }
    }

    #[test]
    fn snapshot_is_bounded_not_a_full_replay() {
        // 10k bytes of churn on a 24-row screen must still snapshot small.
        let mut live = ScreenModel::new(80, 24);
        for i in 0..2000u32 {
            live.feed(format!("\x1b[1;1Hline {i}\r\n").as_bytes());
        }
        assert!(
            live.snapshot().len() < 8192,
            "snapshot should reflect the screen, not the whole history"
        );
    }
}
