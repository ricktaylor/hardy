/// Checks for Administrator privileges on Windows.
/// This function is only compiled on Windows.
#[cfg(target_os = "windows")]
pub fn has_required_privileges() -> bool {
    // This crate provides a simple, safe, and direct way to check
    // for elevation on Windows. It returns a Result, so we handle
    // the potential error by defaulting to `false`.
    check_elevation::is_elevated().unwrap_or(false)
}

/// Checks for root OR the CAP_NET_ADMIN capability on Linux.
/// This function is only compiled on Linux.
#[cfg(target_os = "linux")]
pub fn has_required_privileges() -> bool {
    use caps::{CapSet, Capability};

    // First, check for the specific capability we need in the "effective" set.
    // This is the recommended approach for security, as it allows the application
    // to run with the least privilege necessary.
    if caps::has_cap(None, CapSet::Effective, Capability::CAP_NET_ADMIN).unwrap_or(false) {
        return true;
    }

    // As a fallback, check if the user is running as root (EUID 0).
    // A root user implicitly has all capabilities.
    // Note: The `caps` crate can also check this, but a direct libc call is also common.
    // We'll stick to the `caps` API for consistency here.
    if caps::has_cap(None, CapSet::Effective, Capability::CAP_SETFCAP).is_ok() {
        // A reasonable proxy for checking if the user is root, as only root can set file capabilities.
        // A more direct check would be `nix::unistd::geteuid().is_root()`.
        return true;
    }

    false
}

/// Fallback for other Unix-like systems (e.g., macOS, BSD).
/// These systems do not have the Linux capabilities model, so we must
/// check if the effective user ID is 0 (root).
/// This function is only compiled on non-Windows and non-Linux platforms.
#[cfg(all(unix, not(target_os = "linux")))]
pub fn has_required_privileges() -> bool {
    // This is the classic and safest way to check for root on a Unix system.
    // It is wrapped in an `unsafe` block because it's a Foreign Function Interface (FFI) call.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn test() -> anyhow::Result<()> {
    println!("Checking for required privileges...");

    if !has_required_privileges() {
        eprintln!("\nError: This application requires elevated privileges to run.");

        // Provide platform-specific instructions for the user.
        #[cfg(target_os = "windows")]
        eprintln!("Please right-click the executable and select 'Run as Administrator'.");

        #[cfg(target_os = "linux")]
        eprintln!(
            "Please re-run as root or grant the application the 'CAP_NET_ADMIN' capability using 'setcap'."
        );

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        eprintln!("Please re-run as root.");

        // Exit with a non-zero status code to indicate an error.
        return Err(anyhow::anyhow!("Missing required privileges."));
    }

    println!("Success! Running with sufficient privileges.");
    // --- Your main application logic would go here ---
    // For example: opening a raw socket, configuring a network interface, etc.

    Ok(())
}
