/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::dom::bindings::str::DOMString;

// navigator is an obfuscated API: web sites use it to profile the browser
// and to detect automation. The values below mimic what a Chromium browser
// would report so that Servo is not flagged as a bot or as an unsupported
// browser. `navigator.userAgent` comes from the `user_agent` preference,
// which is also Chromium-compatible by default.

#[expect(non_snake_case)]
pub(crate) fn Product() -> DOMString {
    // Chromium, like every browser, reports "Gecko" here.
    DOMString::from_static("Gecko")
}

#[expect(non_snake_case)]
pub(crate) fn ProductSub() -> DOMString {
    // Chromium reports a frozen Safari-era revision.
    DOMString::from_static("20030107")
}

#[expect(non_snake_case)]
pub(crate) fn Vendor() -> DOMString {
    // Chromium reports "Google Inc."; only WebKit-derived engines report
    // the empty string here.
    DOMString::from_static("Google Inc.")
}

#[expect(non_snake_case)]
pub(crate) fn VendorSub() -> DOMString {
    DOMString::new()
}

#[expect(non_snake_case)]
pub(crate) fn TaintEnabled() -> bool {
    false
}

#[expect(non_snake_case)]
pub(crate) fn AppName() -> DOMString {
    DOMString::from_static("Netscape") // Like Gecko/Webkit
}

#[expect(non_snake_case)]
pub(crate) fn AppCodeName() -> DOMString {
    DOMString::from_static("Mozilla")
}

#[expect(non_snake_case)]
#[cfg(target_os = "windows")]
pub(crate) fn Platform() -> DOMString {
    DOMString::from_static("Win32")
}

#[expect(non_snake_case)]
#[cfg(target_os = "android")]
pub(crate) fn Platform() -> DOMString {
    // Chromium on Android reports the frozen value "Linux armv8l".
    DOMString::from_static("Linux armv8l")
}

#[expect(non_snake_case)]
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn Platform() -> DOMString {
    // Chromium reports the exact architecture on Linux, which is what
    // fingerprinting scripts expect on desktop platforms.
    DOMString::from_static("Linux x86_64")
}

#[expect(non_snake_case)]
#[cfg(target_os = "macos")]
pub(crate) fn Platform() -> DOMString {
    // Chromium always reports "MacIntel", even on Apple-silicon Macs.
    DOMString::from_static("MacIntel")
}

#[expect(non_snake_case)]
#[cfg(target_os = "ios")]
pub(crate) fn Platform() -> DOMString {
    DOMString::from_static("iPhone")
}

#[expect(non_snake_case)]
pub(crate) fn UserAgent(user_agent: &str) -> DOMString {
    DOMString::from(user_agent)
}

#[expect(non_snake_case)]
pub(crate) fn AppVersion(user_agent: &str) -> DOMString {
    // Chromium reports "5.0 (…)": the user-agent string without the
    // leading "Mozilla/" token (i.e. `navigator.userAgent` minus
    // "Mozilla/").
    DOMString::from(match user_agent.strip_prefix("Mozilla/") {
        Some(app_version) => app_version.to_owned(),
        None => user_agent.to_owned(),
    })
}

#[expect(non_snake_case)]
pub(crate) fn Language() -> DOMString {
    DOMString::from(net_traits::get_current_locale().0.clone())
}
