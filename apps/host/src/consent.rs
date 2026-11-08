//! Local consent for an incoming session.
//!
//! The security-critical moment: before a stranger controls this machine the
//! person sitting at it must agree. A native modal dialog is used rather than
//! a console prompt so consent still works when the host runs as a Windows
//! service (the session helper shows the dialog on the user's desktop), and so
//! the scam warning is unmissable. If no dialog can be shown, we fall back to a
//! console confirmation.

use std::net::SocketAddr;

use windows::core::HSTRING;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_SYSTEMMODAL, MB_TOPMOST, MB_YESNO,
};

/// Ask the local user to approve an incoming session. Returns true only on an
/// explicit Yes.
pub fn ask(peer: SocketAddr) -> bool {
    let text = HSTRING::from(format!(
        "Someone at {peer} is asking to control this computer.\n\n\
         Only allow people you contacted yourself and trust. No real company \
         (a bank, Microsoft, a tax office) will ever ask you to install this or \
         read out your connection code. If someone phoned you and told you to \
         do this, it is a scam \u{2014} choose No.\n\n\
         Allow this remote session?"
    ));
    let caption = HSTRING::from("Imperio \u{2014} allow remote access?");
    let style = MB_YESNO | MB_ICONWARNING | MB_SYSTEMMODAL | MB_TOPMOST | MB_SETFOREGROUND;
    // SAFETY: MessageBoxW with valid wide strings and no parent window.
    let result = unsafe { MessageBoxW(None, &text, &caption, style) };
    if result == IDYES {
        return true;
    }
    if result.0 == 0 {
        // The dialog could not be shown (no interactive desktop); fall back.
        tracing::warn!("consent dialog unavailable; falling back to console");
        return console_consent();
    }
    false
}

fn console_consent() -> bool {
    println!("     Press Enter to ACCEPT, or Ctrl+C to refuse...");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok()
}
