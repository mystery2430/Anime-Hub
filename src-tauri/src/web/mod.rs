//! WebView plumbing: session isolation, DNS rebinding check, window policy.

pub mod dns;
/// Connection-time IP pinning for the desktop site WebView. Android uses the
/// Kotlin `DnsPinnedProxy`; the mobile crate has no proxy of its own.
#[cfg(desktop)]
pub mod pinned_proxy;
pub mod session;
pub mod windows;
