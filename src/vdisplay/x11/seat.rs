//! The session on the screen, as logind records it under /run/systemd: whose it is, and whether
//! it is graphical.

/// Where logind keeps seat0, and its sessions.
pub const SEAT0: &str = "/run/systemd/seats/seat0";
pub const SESSIONS: &str = "/run/systemd/sessions";

/// The active session of a seat, and its user, from the seat's file ("ACTIVE=c2",
/// "ACTIVE_UID=1000").
pub fn active(seat: &str) -> (Option<String>, Option<u32>) {
    let value = |key: &str| {
        seat.lines()
            .find_map(|line| line.trim().strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    (
        value("ACTIVE")
            .filter(|id| id.bytes().all(|b| b.is_ascii_alphanumeric()))
            .map(str::to_owned),
        value("ACTIVE_UID").and_then(|uid| uid.parse().ok()),
    )
}

/// Whether a session's file says it is graphical: an X11 or Wayland session, the login screen's
/// included.
pub fn graphical(session: &str) -> bool {
    session
        .lines()
        .any(|line| matches!(line.trim(), "TYPE=x11" | "TYPE=wayland" | "TYPE=mir"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seat_says_who_is_on_the_screen() {
        let seat = "# This is private data. Do not parse.\nIS_SEAT0=1\nCAN_MULTI_SESSION=1\n\
                    CAN_TTY=1\nCAN_GRAPHICAL=1\nACTIVE=c2\nACTIVE_UID=1000\nSESSIONS=c2 1\n\
                    UIDS=1000 113\n";
        assert_eq!(active(seat), (Some("c2".to_owned()), Some(1000)));
        assert_eq!(active("IS_SEAT0=1\nACTIVE=\n"), (None, None));
        assert_eq!(active(""), (None, None));
        // Never a path.
        assert_eq!(active("ACTIVE=../x\nACTIVE_UID=0\n"), (None, Some(0)));
    }

    #[test]
    fn graphical_sessions_are_told_apart() {
        assert!(graphical("UID=113\nUSER=lightdm\nACTIVE=1\nTYPE=x11\nCLASS=greeter\n"));
        assert!(graphical("TYPE=wayland\n"));
        assert!(!graphical("TYPE=tty\nCLASS=user\n"));
        assert!(!graphical(""));
    }
}
