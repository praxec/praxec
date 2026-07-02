//! Platform keyring availability check (Linux/WSL2 only).
//!
//! The upstream ACP runtime eagerly initializes D-Bus Secret Service for
//! OAuth credential storage as part of agent startup. On WSL2, headless
//! Linux dev containers, and fresh server installs, `org.freedesktop.secrets`
//! is typically unavailable — and the runtime fails with a confusing
//! "no secret service" error well after the user has already typed
//! `px acp` and started waiting.
//!
//! [`ensure_keyring_available`] is a pre-flight check: detect the missing
//! service, attempt to start `gnome-keyring-daemon`, and export its env
//! vars so the ACP subprocess inherits them. If the daemon isn't
//! installed, fail fast with an actionable install hint instead of letting
//! the upstream error surface.
//!
//! macOS uses the Keychain (always available); Windows uses the Credential
//! Manager (always available). Both compile to a no-op.
//!
//! ## Reference
//!
//! Adapted from an internal reference CLI implementation, with error messages
//! reworked for `praxec` branding and a slightly tightened spawn-error fallback.

/// Ensure the platform keyring service is running (Linux/WSL2 only).
///
/// Behavior:
/// - If `GNOME_KEYRING_CONTROL` is already set, the service is up; return.
/// - Otherwise spawn `gnome-keyring-daemon --start --components=secrets`.
/// - If the binary is missing entirely, print an install hint and exit 1
///   (the user can't start an ACP session without this; failing fast is
///   kinder than letting the upstream error fire later).
/// - Parse `KEY=VALUE` lines from the daemon's stdout and export them.
///
/// On non-Linux platforms this is a no-op (see the cfg-gated stub below).
#[cfg(target_os = "linux")]
pub fn ensure_keyring_available() {
    use std::process::Command;

    // Already running — nothing to do.
    if std::env::var("GNOME_KEYRING_CONTROL").is_ok() {
        return;
    }

    let output = match Command::new("gnome-keyring-daemon")
        .args(["--start", "--components=secrets"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!(
                    "Error: Platform keyring service unavailable.\n\n\
                     `px acp` requires a Secret Service implementation \
                     for OAuth credential storage (the upstream agent runtime \
                     initializes it eagerly at startup). On Ubuntu/Debian/WSL2, \
                     install it with:\n\n\
                     \tsudo apt install -y gnome-keyring\n\n\
                     Then re-run `px acp`. macOS and Windows ship a \
                     keyring out of the box; this check is Linux-only."
                );
                std::process::exit(1);
            }
            // Other spawn error — downgrade to a warning. The downstream
            // ACP runtime will produce its own (more specific) error if
            // the keyring really isn't usable.
            eprintln!("Warning: Failed to start gnome-keyring-daemon: {e}");
            return;
        }
    };

    if !output.status.success() {
        eprintln!(
            "Warning: gnome-keyring-daemon exited with {}",
            output.status
        );
        return;
    }

    // Parse KEY=VALUE lines from stdout and set them as env vars so the
    // ACP subprocess inherits them.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() && !value.is_empty() {
                // SAFETY: this runs from main() before any tokio runtime
                // spawns workers, so no other thread can race on the env.
                unsafe { std::env::set_var(key, value) };
            }
        }
    }
}

/// No-op stub for non-Linux platforms. macOS Keychain and Windows
/// Credential Manager are always available; no pre-flight needed.
#[cfg(not(target_os = "linux"))]
pub fn ensure_keyring_available() {}
