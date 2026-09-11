// Checks for Administrator privileges on Windows.
// This function is only compiled on Windows.
#[cfg(target_os = "windows")]
pub fn has_required_privileges() -> bool {
    // This crate provides a simple, safe, and direct way to check
    // for elevation on Windows. It returns a Result, so we handle
    // the potential error by defaulting to `false`.
    check_elevation::is_elevated().unwrap_or(false)
}

// Checks for root OR the CAP_NET_ADMIN capability on Linux.
// This function is only compiled on Linux.
#[cfg(target_os = "linux")]
pub fn has_required_privileges() -> bool {
    use caps::{CapSet, Capability};

    // First, check for the specific capability we need in the "effective" set.
    // This is the recommended approach for security, as it allows the application
    // to run with the least privilege necessary.
    if caps::has_cap(None, CapSet::Effective, Capability::CAP_NET_ADMIN).unwrap_or(false) {
        return true;
    }

    // As a fallback, check whether the process is effectively root: only
    // root holds CAP_SETFCAP by default, so an effective CAP_SETFCAP is a
    // reasonable proxy for EUID 0.
    if caps::has_cap(None, CapSet::Effective, Capability::CAP_SETFCAP).unwrap_or(false) {
        return true;
    }

    false
}

// Fallback for other Unix-like systems (e.g., macOS, BSD).
// These systems do not have the Linux capabilities model, so we must
// check if the effective user ID is 0 (root).
// This function is only compiled on non-Windows and non-Linux platforms.
#[cfg(all(unix, not(target_os = "linux")))]
pub fn has_required_privileges() -> bool {
    // This is the classic and safest way to check for root on a Unix system.
    // It is wrapped in an `unsafe` block because it's a Foreign Function Interface (FFI) call.
    unsafe { libc::geteuid() == 0 }
}

// The function must agree with the platform's ground truth for the
// privileges it claims to check, whatever privileges the test runner
// happens to have: the test passes for both privileged and unprivileged
// runners, but fails if the function's answer diverges from the platform
// query it is contracted to reflect.
#[test]
fn agrees_with_platform_ground_truth() {
    let actual = has_required_privileges();

    #[cfg(target_os = "linux")]
    let expected = {
        use caps::{CapSet, Capability};
        caps::has_cap(None, CapSet::Effective, Capability::CAP_NET_ADMIN).unwrap_or(false)
            || caps::has_cap(None, CapSet::Effective, Capability::CAP_SETFCAP).unwrap_or(false)
    };

    #[cfg(target_os = "windows")]
    let expected = check_elevation::is_elevated().unwrap_or(false);

    #[cfg(all(unix, not(target_os = "linux")))]
    let expected = unsafe { libc::geteuid() == 0 };

    assert_eq!(
        actual, expected,
        "has_required_privileges() disagrees with the platform privilege query"
    );
}
